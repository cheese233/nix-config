//! Per-child worker: sockets, QUIC connection, and the single-threaded
//! epoll event loop. Everything in the request path runs on one stack —
//! no channels, no context switches, no shared state.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use noq_proto as proto;

use crate::bootstrap::{Bootstrap, ResolveState};
use crate::dns;
use crate::event::{Poller, TOKEN_DNS, TOKEN_QUIC, TOKEN_SIGNAL, TOKEN_TIMER};
use crate::h3::{self, H3, H3Event};
use crate::quic::{self, Quic};
use crate::url::HttpsUrl;

/// GET is used for wire queries up to this size; larger use POST.
const GET_MAX_DNS_LEN: usize = 1400;
/// Max wire size we accept from local clients.
const MAX_DNS_LEN: usize = 4096;
/// Bootstrap re-resolution retry delay after a failure.
const BOOTSTRAP_RETRY: Duration = Duration::from_secs(60);
/// Reconnect backoff schedule cap.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);
/// QUIC keep-alive ping interval.
const KEEP_ALIVE: Duration = Duration::from_secs(15);
/// QUIC idle timeout we advertise.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct WorkerConfig {
    pub listen: SocketAddr,
    pub upstream: HttpsUrl,
    pub bootstrap_dns: IpAddr,
    pub token: Option<Arc<str>>,
    pub timeout: Duration,
    pub pad: bool,
    pub busy_poll: bool,
    pub spin: bool,
    pub mlockall: bool,
}

struct Pending {
    /// Full wire query (kept so SERVFAIL can echo the question section).
    query: Vec<u8>,
    peer: SocketAddr,
    deadline: Instant,
}

/// A validated query received while the connection is handshaking
/// (streams can't open until the server's transport parameters arrive,
/// unless 0-RTT restored them).
struct QueuedQuery {
    query: Vec<u8>,
    peer: SocketAddr,
    deadline: Instant,
}

/// Max queries queued while handshaking; overflow gets SERVFAIL.
const MAX_QUEUED: usize = 64;

/// Open the DNS listen socket: SO_REUSEPORT, non-blocking, big buffers.
fn bind_dns_socket(addr: SocketAddr, busy_poll: bool) -> io::Result<UdpSocket> {
    use nix::sys::socket::*;
    let family = if addr.is_ipv6() {
        AddressFamily::Inet6
    } else {
        AddressFamily::Inet
    };
    let fd = socket(
        family,
        SockType::Datagram,
        SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    setsockopt(&fd, sockopt::ReusePort, &true)?;
    setsockopt(&fd, sockopt::RcvBuf, &(4 * 1024 * 1024))?;
    setsockopt(&fd, sockopt::SndBuf, &(4 * 1024 * 1024))?;
    if busy_poll {
        // 50µs busy-poll budget: latency wins on multi-core boxes with spare cores.
        // SO_BUSY_POLL has no nix wrapper; call libc directly.
        unsafe {
            nix::libc::setsockopt(
                fd.as_raw_fd(),
                nix::libc::SOL_SOCKET,
                nix::libc::SO_BUSY_POLL as i32,
                &50i32 as *const i32 as *const nix::libc::c_void,
                std::mem::size_of::<i32>() as nix::libc::socklen_t,
            );
        }
    }
    match addr {
        SocketAddr::V4(v4) => bind(fd.as_raw_fd(), &SockaddrIn::from(v4))?,
        SocketAddr::V6(v6) => bind(fd.as_raw_fd(), &SockaddrIn6::from(v6))?,
    }
    Ok(fd.into())
}

/// Resolve the upstream host, IPv6 first (works well with NAT64/DNS64).
fn resolve_upstream(
    bootstrap: &Bootstrap,
    host: &str,
    port: u16,
) -> io::Result<(ResolveState, Vec<SocketAddr>)> {
    let state = bootstrap
        .resolve(host)
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, e.to_string()))?;
    let mut v6: Vec<SocketAddr> = state
        .addrs
        .iter()
        .filter(|a| a.is_ipv6())
        .map(|&a| SocketAddr::new(a, port))
        .collect();
    let mut v4: Vec<SocketAddr> = state
        .addrs
        .iter()
        .filter(|a| a.is_ipv4())
        .map(|&a| SocketAddr::new(a, port))
        .collect();
    v6.append(&mut v4);
    Ok((state, v6))
}

/// Bind the QUIC client UDP socket matching the remote's family.
fn bind_quic_socket(remote: &SocketAddr) -> io::Result<UdpSocket> {
    let any: SocketAddr = if remote.is_ipv6() {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    };
    let sock = UdpSocket::bind(any)?;
    sock.set_nonblocking(true)?;
    Ok(sock)
}

/// One child process. Never returns under normal operation.
pub fn run(cfg: WorkerConfig) -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::mman::{mlockall, MlockAllFlags};

    if cfg.mlockall {
        let _ = mlockall(MlockAllFlags::MCL_CURRENT | MlockAllFlags::MCL_FUTURE);
    }

    let dns_sock = bind_dns_socket(cfg.listen, cfg.busy_poll)?;
    log::info!("worker {} listening on {dns_sock:?}", std::process::id());

    let bootstrap = Bootstrap::new(cfg.bootstrap_dns);
    let (mut resolve_state, mut remotes) = resolve_upstream(
        &bootstrap,
        &cfg.upstream.host,
        cfg.upstream.port,
    )?;
    let mut remote_idx = 0usize;

    let quic_sock = bind_quic_socket(&remotes[remote_idx])?;
    let udp_state = Quic::init_socket(&quic_sock)?;
    let client_config = quic::build_client_config(KEEP_ALIVE, IDLE_TIMEOUT)?;
    let mut quic = Quic::new(client_config, cfg.upstream.host.clone(), udp_state);

    let poller = Poller::new()?;
    poller.add_socket(&dns_sock, TOKEN_DNS)?;
    poller.add_socket(&quic_sock, TOKEN_QUIC)?;

    let mut h3 = H3::new();
    let mut pending: HashMap<u64, Pending> = HashMap::new();
    let mut queue: std::collections::VecDeque<QueuedQuery> = Default::default();
    let mut h3_events: Vec<H3Event> = Vec::with_capacity(8);
    let mut goaway = false;
    let mut reconnect_at: Option<Instant> = None;
    let mut backoff = Duration::ZERO;
    let mut refresh_due = Instant::now() + Duration::from_secs(3600);
    let mut req_buf: Vec<u8> = Vec::with_capacity(2048);

    let mut now = Instant::now();
    // Streams can open immediately only with remembered (0-RTT) transport
    // parameters; otherwise wait for the Connected event.
    let mut h3_ready = connect_and_preamble(&mut quic, now, remotes[remote_idx], &quic_sock)?;

    let mut events = [nix::sys::epoll::EpollEvent::empty(); 16];
    let mut shutdown = false;

    while !shutdown {
        // ── Arm the timer to the earliest deadline ──
        let mut next = quic.next_timeout();
        if let Some(t) = reconnect_at {
            next = Some(next.map_or(t, |n| n.min(t)));
        }
        if let Some(d) = pending.values().map(|p| p.deadline).min() {
            next = Some(next.map_or(d, |n| n.min(d)));
        }
        if let Some(q) = queue.front() {
            next = Some(next.map_or(q.deadline, |n| n.min(q.deadline)));
        }
        next = Some(next.map_or(refresh_due, |n| n.min(refresh_due)));
        poller.arm_timer(next)?;

        let n = poller.wait(&mut events, cfg.spin)?;
        now = Instant::now();

        for ev in &events[..n] {
            match ev.data() {
                TOKEN_DNS => drain_dns(
                    &cfg,
                    &dns_sock,
                    &quic_sock,
                    &mut quic,
                    &mut h3,
                    &mut pending,
                    &mut queue,
                    &mut req_buf,
                    goaway,
                    h3_ready,
                    now,
                ),
                TOKEN_QUIC => {
                    if let Err(e) = quic.poll_socket(now, &quic_sock) {
                        log::warn!("quic socket error: {e}");
                    }
                    process_quic_events(
                        &cfg,
                        &dns_sock,
                        &quic_sock,
                        &mut quic,
                        &mut h3,
                        &mut pending,
                        &mut queue,
                        &mut h3_events,
                        &mut req_buf,
                        &mut goaway,
                        &mut h3_ready,
                        &mut reconnect_at,
                        &mut backoff,
                        now,
                    );
                }
                TOKEN_TIMER => {
                    poller.drain_timer();
                    quic.handle_timeout(now, &quic_sock)?;
                    housekeeping(
                        &cfg,
                        &dns_sock,
                        &quic_sock,
                        &bootstrap,
                        &mut quic,
                        &mut h3,
                        &mut pending,
                        &mut queue,
                        &mut resolve_state,
                        &mut remotes,
                        &mut remote_idx,
                        &mut goaway,
                        &mut h3_ready,
                        &mut reconnect_at,
                        &mut refresh_due,
                        now,
                    );
                }
                TOKEN_SIGNAL => {
                    if poller.shutdown_signaled() {
                        log::info!("worker {} shutting down", std::process::id());
                        shutdown = true;
                    }
                }
                _ => {}
            }
        }

        // Connection fully drained → schedule reconnect.
        if quic.has_conn() && quic.is_drained() {
            fail_all_pending(&dns_sock, &mut pending);
            quic.drop_conn();
            h3 = H3::new();
            goaway = false;
            h3_ready = false;
            reconnect_at.get_or_insert(now + backoff);
        }

        if let Err(e) = quic.flush(now, &quic_sock) {
            log::debug!("flush: {e}");
        }
    }

    // Graceful shutdown: fail in-flight queries, close the connection.
    fail_all_pending(&dns_sock, &mut pending);
    quic.close(now, &quic_sock);
    Ok(())
}

/// Establish the QUIC connection; if 0-RTT restored the transport
/// parameters, send the H3 client preamble immediately. Returns true when
/// request streams may be opened right away.
fn connect_and_preamble(
    quic: &mut Quic,
    now: Instant,
    remote: SocketAddr,
    sock: &UdpSocket,
) -> Result<bool, Box<dyn std::error::Error>> {
    quic.connect(now, remote, sock)?;
    let ready = quic.has_0rtt();
    if ready {
        send_preamble(quic);
    }
    quic.flush(now, sock)?;
    Ok(ready)
}

/// Send the H3 client preamble on the first client uni stream.
/// Must be called before opening any request stream.
fn send_preamble(quic: &mut Quic) {
    match quic.open_control_uni() {
        Some(ctrl) => {
            if let Err(e) = quic.write_all(ctrl, &h3::client_preamble()) {
                log::warn!("write preamble: {e}");
            }
            // Control stream stays open for the connection lifetime (no FIN).
        }
        None => log::warn!("could not open control stream"),
    }
}

/// Read and dispatch all waiting DNS queries.
#[allow(clippy::too_many_arguments)]
fn drain_dns(
    cfg: &WorkerConfig,
    dns_sock: &UdpSocket,
    quic_sock: &UdpSocket,
    quic: &mut Quic,
    h3: &mut H3,
    pending: &mut HashMap<u64, Pending>,
    queue: &mut std::collections::VecDeque<QueuedQuery>,
    req_buf: &mut Vec<u8>,
    goaway: bool,
    h3_ready: bool,
    now: Instant,
) {
    let mut buf = [0u8; MAX_DNS_LEN];
    loop {
        let (n, peer) = match dns_sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        };
        let query = &buf[..n];
        if dns::validate_query(query).is_err() {
            continue;
        }

        if goaway || !quic.has_conn() {
            send_servfail(dns_sock, query, peer);
            continue;
        }
        if !h3_ready {
            // Handshake in progress: queue briefly (the handshake is ~1 RTT).
            if queue.len() >= MAX_QUEUED {
                send_servfail(dns_sock, query, peer);
            } else {
                queue.push_back(QueuedQuery {
                    query: query.to_vec(),
                    peer,
                    deadline: now + cfg.timeout,
                });
            }
            continue;
        }
        dispatch_query(cfg, dns_sock, quic, h3, pending, req_buf, query, peer, now);
    }
    let _ = quic.flush(now, quic_sock);
}

/// Flush queued queries once the connection is ready.
#[allow(clippy::too_many_arguments)]
fn flush_queue(
    cfg: &WorkerConfig,
    dns_sock: &UdpSocket,
    quic_sock: &UdpSocket,
    quic: &mut Quic,
    h3: &mut H3,
    pending: &mut HashMap<u64, Pending>,
    queue: &mut std::collections::VecDeque<QueuedQuery>,
    req_buf: &mut Vec<u8>,
    now: Instant,
) {
    while let Some(q) = queue.pop_front() {
        if q.deadline <= now {
            send_servfail(dns_sock, &q.query, q.peer);
            continue;
        }
        dispatch_query(cfg, dns_sock, quic, h3, pending, req_buf, &q.query, q.peer, now);
    }
    let _ = quic.flush(now, quic_sock);
}

/// Submit one validated DNS query to the DoH upstream over a new H3 stream.
#[allow(clippy::too_many_arguments)]
fn dispatch_query(
    cfg: &WorkerConfig,
    dns_sock: &UdpSocket,
    quic: &mut Quic,
    h3: &mut H3,
    pending: &mut HashMap<u64, Pending>,
    req_buf: &mut Vec<u8>,
    query: &[u8],
    peer: SocketAddr,
    now: Instant,
) {
    let n = query.len();
    let stream = match quic.open_bi() {
        Some(s) => s,
        None => {
            send_servfail(dns_sock, query, peer);
            return;
        }
    };
    let stream_idx: u64 = stream.into();

    req_buf.clear();
    let use_get = n <= GET_MAX_DNS_LEN;
    let mut query_vec;
    let query_ref = if cfg.pad || use_get {
        query_vec = query.to_vec();
        if cfg.pad {
            dns::pad_query(&mut query_vec, 128);
        }
        if use_get && query_vec.len() >= 2 {
            // RFC 8484 §4.1: zero the DNS ID for GET cache-friendliness.
            query_vec[0] = 0;
            query_vec[1] = 0;
        }
        &query_vec[..]
    } else {
        query
    };

    if use_get {
        let b64_len = crate::base64url::encoded_len(query_ref.len());
        let mut b64 = vec![0u8; b64_len];
        crate::base64url::encode_into(query_ref, &mut b64);
        let sep = if cfg.upstream.path.contains('?') { '&' } else { '?' };
        let mut path = String::with_capacity(cfg.upstream.path.len() + 5 + b64_len);
        path.push_str(&cfg.upstream.path);
        path.push(sep);
        path.push_str("dns=");
        path.push_str(std::str::from_utf8(&b64).unwrap());
        h3::encode_request(
            req_buf,
            "GET",
            &cfg.upstream.authority,
            &path,
            cfg.token.as_deref(),
            None,
        );
    } else {
        h3::encode_request(
            req_buf,
            "POST",
            &cfg.upstream.authority,
            &cfg.upstream.path,
            cfg.token.as_deref(),
            Some(query_ref),
        );
    }

    if let Err(e) = quic.write_all(stream, req_buf) {
        log::debug!("write_all stream {stream_idx}: {e}");
        send_servfail(dns_sock, query, peer);
        return;
    }
    if let Err(e) = quic.finish_stream(stream) {
        log::debug!("finish_stream {stream_idx}: {e}");
        send_servfail(dns_sock, query, peer);
        return;
    }
    log::trace!("dispatch stream {stream_idx}: {} request bytes: {:02x?}", req_buf.len(), &req_buf[..req_buf.len().min(64)]);
    h3.register_request(stream_idx);
    pending.insert(
        stream_idx,
        Pending {
            query: query.to_vec(),
            peer,
            deadline: now + cfg.timeout,
        },
    );
}

/// Drain QUIC application events and route stream data through H3.
#[allow(clippy::too_many_arguments)]
fn process_quic_events(
    cfg: &WorkerConfig,
    dns_sock: &UdpSocket,
    quic_sock: &UdpSocket,
    quic: &mut Quic,
    h3: &mut H3,
    pending: &mut HashMap<u64, Pending>,
    queue: &mut std::collections::VecDeque<QueuedQuery>,
    h3_events: &mut Vec<H3Event>,
    req_buf: &mut Vec<u8>,
    goaway: &mut bool,
    h3_ready: &mut bool,
    reconnect_at: &mut Option<Instant>,
    backoff: &mut Duration,
    now: Instant,
) {
    for ev in quic.poll_events() {
        match ev {
            proto::Event::Connected => {
                *backoff = Duration::ZERO;
                *reconnect_at = None;
                log::info!("quic: connected (0-rtt accepted: {})", quic.zero_rtt_accepted);
                if !*h3_ready {
                    // Transport parameters arrived: open the control stream
                    // first, then flush queued queries.
                    send_preamble(quic);
                    *h3_ready = true;
                    flush_queue(
                        cfg, dns_sock, quic_sock, quic, h3, pending, queue, req_buf, now,
                    );
                }
            }
            proto::Event::ConnectionLost { reason } => {
                log::warn!("quic: connection lost: {reason}");
                fail_all_pending(dns_sock, pending);
                quic.drop_conn();
                *h3 = H3::new();
                *goaway = false;
                *h3_ready = false;
                // Exponential backoff: 0 → 100ms → 200 → … → 5s cap.
                let delay = *backoff;
                *reconnect_at = Some(now + delay);
                *backoff = (*backoff * 2 + Duration::from_millis(100)).min(RECONNECT_BACKOFF_MAX);
            }
            proto::Event::Stream(proto::StreamEvent::Readable { id }) => {
                let idx: u64 = id.into();
                let (eof, read_err) = {
                    let ev_acc = &mut *h3_events;
                    quic.read_stream(id, |chunk| {
                        log::trace!("stream {idx} rx {} bytes: {:02x?}", chunk.len(), &chunk[..chunk.len().min(300)]);
                        h3.feed(idx, chunk, ev_acc);
                    })
                };
                if read_err {
                    log::debug!("stream {idx} read error");
                    h3.reset(idx, h3_events);
                } else if eof {
                    // Stream EOF (all data + FIN consumed) — response complete.
                    h3.finish(idx, h3_events);
                }
            }
            proto::Event::Stream(proto::StreamEvent::Finished { .. }) => {
                // Send-side event: our FIN was acknowledged (or stream
                // stopped). Irrelevant for response completion — the read
                // side's EOF is what ends a response.
            }
            proto::Event::Stream(proto::StreamEvent::Stopped { id, error_code }) => {
                // Peer sent STOP_SENDING for our request stream; the read
                // side is unaffected — keep reading until EOF/reset.
                log::trace!("stream {} stopped by peer, code {:?}", u64::from(id), error_code);
            }
            _ => {}
        }
    }

    // Apply H3 events.
    for ev in h3_events.drain(..) {
        match ev {
            H3Event::Response { stream, body } => {
                log::trace!("h3: stream {stream} response {} bytes", body.len());
                if let Some(p) = pending.remove(&stream) {
                    let mut body = body;
                    if body.len() >= 2 && p.query.len() >= 2 {
                        body[0] = p.query[0];
                        body[1] = p.query[1];
                    }
                    if let Err(e) = dns_sock.send_to(&body, p.peer) {
                        log::debug!("dns send_to {}: {e}", p.peer);
                    }
                }
            }
            H3Event::Failed { stream } => {
                log::debug!("h3: stream {stream} failed");
                if let Some(p) = pending.remove(&stream) {
                    send_servfail(dns_sock, &p.query, p.peer);
                }
            }
            H3Event::Goaway => {
                // Skip stale GOAWAYs arriving after the connection was
                // already torn down (events are drained in one batch).
                if quic.has_conn() {
                    log::info!("h3: GOAWAY received, draining");
                    *goaway = true;
                    if pending.is_empty() {
                        *reconnect_at = Some(now);
                    }
                } else {
                    log::trace!("h3: ignoring stale GOAWAY (connection gone)");
                }
            }
        }
    }
}

/// Periodic tasks: request expiry, bootstrap refresh, reconnect.
#[allow(clippy::too_many_arguments)]
fn housekeeping(
    cfg: &WorkerConfig,
    dns_sock: &UdpSocket,
    quic_sock: &UdpSocket,
    bootstrap: &Bootstrap,
    quic: &mut Quic,
    h3: &mut H3,
    pending: &mut HashMap<u64, Pending>,
    queue: &mut std::collections::VecDeque<QueuedQuery>,
    resolve_state: &mut ResolveState,
    remotes: &mut Vec<SocketAddr>,
    remote_idx: &mut usize,
    goaway: &mut bool,
    h3_ready: &mut bool,
    reconnect_at: &mut Option<Instant>,
    refresh_due: &mut Instant,
    now: Instant,
) {
    // ── Expire timed-out requests with SERVFAIL ──
    let expired: Vec<u64> = pending
        .iter()
        .filter(|(_, p)| p.deadline <= now)
        .map(|(&k, _)| k)
        .collect();
    for k in expired {
        if let Some(p) = pending.remove(&k) {
            send_servfail(dns_sock, &p.query, p.peer);
        }
    }

    // ── Bootstrap TTL refresh (stale-while-revalidate) ──
    if *refresh_due <= now || !resolve_state.is_fresh() {
        match bootstrap.resolve(&cfg.upstream.host) {
            Ok(state) => {
                let mut v6: Vec<SocketAddr> = state
                    .addrs
                    .iter()
                    .filter(|a| a.is_ipv6())
                    .map(|&a| SocketAddr::new(a, cfg.upstream.port))
                    .collect();
                let mut v4: Vec<SocketAddr> = state
                    .addrs
                    .iter()
                    .filter(|a| a.is_ipv4())
                    .map(|&a| SocketAddr::new(a, cfg.upstream.port))
                    .collect();
                v6.append(&mut v4);
                if !v6.is_empty() {
                    *remotes = v6;
                    *remote_idx %= remotes.len();
                    *refresh_due = state.expires_at;
                }
                *resolve_state = state;
            }
            Err(e) => {
                log::warn!("bootstrap refresh failed (keeping stale): {e}");
                *refresh_due = now + BOOTSTRAP_RETRY;
            }
        }
    }

    // ── Expire stale queued (never-sent) queries ──
    while let Some(q) = queue.front() {
        if q.deadline <= now {
            let q = queue.pop_front().unwrap();
            send_servfail(dns_sock, &q.query, q.peer);
        } else {
            break;
        }
    }

    // ── GOAWAY fully drained → reconnect ──
    if *goaway && pending.is_empty() {
        *reconnect_at = Some(now);
    }

    // ── Reconnect ──
    if let Some(t) = *reconnect_at {
        if t <= now {
            // If a connection still exists (GOAWAY drained or stale), close
            // it before reconnecting — otherwise the reconnect branch below
            // would no-op and the timer would spin on a past deadline.
            if quic.has_conn() {
                quic.close(now, quic_sock);
                quic.drop_conn();
                *h3 = H3::new();
                *h3_ready = false;
                *goaway = false;
            }
            // Pick the next remote matching the QUIC socket's family.
            let sock_is_v6 = quic_sock
                .local_addr()
                .map(|a| a.is_ipv6())
                .unwrap_or(false);
            let mut tried = 0;
            let remote = loop {
                *remote_idx = (*remote_idx + 1) % remotes.len().max(1);
                let r = remotes[*remote_idx];
                tried += 1;
                if r.is_ipv6() == sock_is_v6 || tried >= remotes.len() {
                    break r;
                }
            };
            if remote.is_ipv6() != sock_is_v6 {
                log::error!("no remote matching socket family; retrying in 1s");
                *reconnect_at = Some(now + Duration::from_secs(1));
                return;
            }
            log::info!("quic: reconnecting to {remote}");
            match connect_and_preamble(quic, now, remote, quic_sock) {
                Ok(ready) => {
                    *h3_ready = ready;
                    *reconnect_at = None;
                }
                Err(e) => {
                    log::warn!("reconnect failed: {e}");
                    *reconnect_at = Some(now + Duration::from_secs(1));
                }
            }
        }
    }
}

fn send_servfail(sock: &UdpSocket, query: &[u8], peer: SocketAddr) {
    let mut out = Vec::with_capacity(query.len());
    if dns::build_servfail(query, &mut out) {
        let _ = sock.send_to(&out, peer);
    }
}

fn fail_all_pending(sock: &UdpSocket, pending: &mut HashMap<u64, Pending>) {
    for (_, p) in pending.drain() {
        send_servfail(sock, &p.query, p.peer);
    }
}
