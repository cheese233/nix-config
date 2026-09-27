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

use crate::dns;
use crate::event::{Poller, TOKEN_DNS, TOKEN_QUIC, TOKEN_SIGNAL, TOKEN_TIMER};
use crate::h3::{self, H3Event, H3};
use crate::quic::{self, Quic};
use crate::shared::{
    self, score_from_rtt, ScoreSample, UpstreamSet, FLAG_CONNECTED, FLAG_HANDSHAKING, FLAG_ZERO_RTT,
};
use crate::url::HttpsUrl;

/// GET is used for wire queries up to this size; larger use POST.
const GET_MAX_DNS_LEN: usize = 1400;
/// Max wire size we accept from local clients.
const MAX_DNS_LEN: usize = 4096;
/// Reconnect backoff schedule cap.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);
/// QUIC keep-alive ping interval.
///
/// Deliberately much shorter than the peer's idle timeout: with a lossy path a
/// single dropped PING must not let the peer's idle timer expire, and the
/// observed graceful closes (`closed by peer: 256`) arrived exactly one interval
/// apart — i.e. at the first PING — which is what a 15s interval racing a ~30s
/// idle timeout looks like when PINGs are being dropped.
const KEEP_ALIVE: Duration = Duration::from_secs(5);
/// QUIC idle timeout we advertise.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// How often a worker publishes its quality measurement.
const SCORE_INTERVAL: Duration = Duration::from_secs(2);
/// Minimum time between two migrations of the same worker. A migration costs a
/// full handshake, so staying put is the default; this floor keeps a flapping
/// weight set from re-handshaking continuously.
const MIGRATE_MIN_INTERVAL: Duration = Duration::from_secs(180);
/// Per-worker delay after a new revision before a move is considered,
/// multiplied by the worker index — keeps the fleet from migrating at once.
const MIGRATE_STAGGER: Duration = Duration::from_secs(5);
/// Move only when the newly assigned address measures this much better than
/// the current one (percent of quality). Hysteresis against weight churn.
const MIGRATE_RATIO_PCT: u64 = 150;
/// A worker whose connection keeps failing may jump to the best alternative
/// this soon after its previous move (the normal floor is `MIGRATE_MIN_INTERVAL`).
const FAIL_MIGRATE_INTERVAL: Duration = Duration::from_secs(10);

pub struct WorkerConfig {
    pub listen: SocketAddr,
    pub upstream: HttpsUrl,
    pub token: Option<Arc<str>>,
    pub timeout: Duration,
    pub pad: bool,
    pub busy_poll: bool,
    pub spin: bool,
    pub mlockall: bool,
    /// Inherited fd of the supervisor's shared resolution page (mapped
    /// read-only here). The supervisor is the sole writer and keeps it
    /// fresh — children never do blocking DNS.
    pub shm_fd: std::os::fd::RawFd,
    /// Inherited fd of the shared worker-score page (mapped read-write here:
    /// each child owns exactly one slot and is its only writer).
    pub score_fd: std::os::fd::RawFd,
    /// Worker index: selects the score slot and the weighted address slot.
    pub child_idx: usize,
}

struct Pending {
    /// Full wire query (kept so SERVFAIL can echo the question section).
    query: Vec<u8>,
    peer: SocketAddr,
    deadline: Instant,
    /// When the request was dispatched, for end-to-end latency measurement.
    started: Instant,
}

/// Rolling quality measurement of the address this worker is using.
///
/// Measured at the request level (dispatch → response) rather than from QUIC
/// internals: that is the number the weight actually cares about, and it needs
/// no extra protocol plumbing.
struct Quality {
    /// EWMA of successful request latency, microseconds (0 = nothing yet).
    ewma_us: u64,
    /// Completed / failed requests, for diagnostics in the score slot.
    ok: u32,
    failed: u32,
}

impl Quality {
    fn new() -> Self {
        Self {
            ewma_us: 0,
            ok: 0,
            failed: 0,
        }
    }

    fn on_success(&mut self, latency: Duration) {
        let us = latency.as_micros().min(u32::MAX as u128) as u64;
        self.ok = self.ok.saturating_add(1);
        if self.ewma_us == 0 {
            self.ewma_us = us;
        } else {
            // 1/4 gain: responsive to a degraded path, not to single outliers.
            self.ewma_us = (self.ewma_us * 3 + us) / 4;
        }
    }

    fn on_failure(&mut self) {
        self.failed = self.failed.saturating_add(1);
    }

    /// Build the sample to publish.
    ///
    /// The score is just the inverse of the measured RTT, so nothing here has
    /// to decide what "healthy" means: a worker that cannot serve traffic has
    /// no successful sample and therefore scores 0, which the supervisor reads
    /// as "unmeasured" and floors. A lossy path needs no extra term either —
    /// its successes are the retransmitted, slow ones.
    fn sample(&self, addr: Option<IpAddr>, flags: u32) -> ScoreSample {
        ScoreSample {
            addr,
            score: score_from_rtt(self.ewma_us.min(u32::MAX as u64) as u32),
            ok: self.ok,
            fail: self.failed,
            flags,
            updated_mono_secs: 0,
        }
    }

    fn reset_window(&mut self) {
        self.ok = 0;
        self.failed = 0;
    }
}

/// Per-worker mutable state that the request path, the score publisher and the
/// migration policy all share.
struct Runtime {
    /// Worker index (score slot, weighted slot, migration stagger).
    child_idx: usize,
    /// Consecutive failed connects/losses on the current address.
    fail_streak: u32,
    /// Latest upstream set published by the supervisor.
    set: UpstreamSet,
    /// Address this worker is currently connected to.
    my_remote: SocketAddr,
    /// Slot index of `my_remote` within `set`.
    slot: usize,
    /// Last time this worker changed address (rate-limits handshakes).
    last_migrate: Instant,
    /// Rolling request-level quality of `my_remote`.
    quality: Quality,
    /// Last score publication.
    last_score: Instant,
}

impl Runtime {
    fn new(set: UpstreamSet, cfg: &WorkerConfig, now: Instant) -> Self {
        // The slot may be of either family: the QUIC socket is dual-stack, so
        // this worker can be moved between an IPv4 and an IPv6 upstream later
        // without rebinding anything.
        let slot = set.slot_for(cfg.child_idx).unwrap_or(0);
        let my_remote = SocketAddr::new(set.addrs[slot], cfg.upstream.port);
        Self {
            child_idx: cfg.child_idx,
            fail_streak: 0,
            set,
            my_remote,
            slot,
            last_migrate: now,
            quality: Quality::new(),
            last_score: now,
        }
    }

    /// Weight the supervisor currently assigns to the address we are on.
    fn current_weight(&self) -> u32 {
        self.set.weight_of(self.my_remote.ip())
    }
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

/// Bind the QUIC client UDP socket: dual-stack when the host allows it.
///
/// A worker owns one socket for its whole life, and the addresses it may be
/// handed are chosen by measured quality rather than by family — so the socket
/// must be able to reach both. `IPV6_V6ONLY` has to be cleared *before* bind(),
/// which is why this builds the socket by hand instead of `UdpSocket::bind`.
///
/// Hosts without IPv6 fall back to an IPv4 socket; then only IPv4 upstream
/// addresses are reachable, which the scoring handles on its own (the others
/// never produce a score and sink to the probe floor).
fn bind_quic_socket() -> io::Result<UdpSocket> {
    match bind_dual_stack() {
        Ok(sock) => Ok(sock),
        Err(e) => {
            log::warn!("dual-stack QUIC socket unavailable ({e}); falling back to IPv4 only");
            let sock = UdpSocket::bind("0.0.0.0:0")?;
            sock.set_nonblocking(true)?;
            Ok(sock)
        }
    }
}

fn bind_dual_stack() -> io::Result<UdpSocket> {
    use nix::sys::socket::*;
    let fd = socket(
        AddressFamily::Inet6,
        SockType::Datagram,
        SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    setsockopt(&fd, sockopt::Ipv6V6Only, &false)?;
    let any: SocketAddr = "[::]:0".parse().unwrap();
    match any {
        SocketAddr::V6(v6) => bind(fd.as_raw_fd(), &SockaddrIn6::from(v6))?,
        SocketAddr::V4(_) => unreachable!("literal is IPv6"),
    }
    Ok(fd.into())
}

/// One child process. Never returns under normal operation.
pub fn run(cfg: WorkerConfig) -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::mman::{mlockall, MlockAllFlags};

    if cfg.mlockall {
        let _ = mlockall(MlockAllFlags::MCL_CURRENT | MlockAllFlags::MCL_FUTURE);
    }

    let dns_sock = bind_dns_socket(cfg.listen, cfg.busy_poll)?;
    log::info!("worker {} listening on {dns_sock:?}", std::process::id());

    // Read upstream addresses and their selection weights from the
    // supervisor's shared page (read-only here).
    let mut shm = shared::ResolveReader::map_readonly(cfg.shm_fd)?;
    let set = shm.read_initial();
    if set.addrs.is_empty() {
        return Err("no upstream addresses in shared memory".into());
    }
    // Claim this worker's own score slot (single writer per slot).
    let scores = shared::ScoreWriter::map(cfg.score_fd, cfg.child_idx)?;

    let start_now = Instant::now();
    let mut rt = Runtime::new(set, &cfg, start_now);
    log::info!(
        "worker {} on {}/{} starting on {}",
        std::process::id(),
        cfg.child_idx,
        rt.set.workers,
        rt.my_remote
    );

    let quic_sock = bind_quic_socket()?;
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
    let mut req_buf: Vec<u8> = Vec::with_capacity(2048);

    let mut now = Instant::now();
    // Streams can open immediately only with remembered (0-RTT) transport
    // parameters; otherwise wait for the Connected event.
    let mut h3_ready = connect_and_preamble(&mut quic, now, rt.my_remote, &quic_sock)?;

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
                    &mut rt,
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
                        &mut rt,
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
                        &mut shm,
                        &scores,
                        &mut quic,
                        &mut h3,
                        &mut pending,
                        &mut queue,
                        &mut rt,
                        &mut goaway,
                        &mut h3_ready,
                        &mut reconnect_at,
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
            rt.fail_streak = rt.fail_streak.saturating_add(1);
            fail_all_pending(&dns_sock, &mut pending, &mut rt);
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
    fail_all_pending(&dns_sock, &mut pending, &mut rt);
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
    rt: &mut Runtime,
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
            rt.quality.on_failure();
            send_servfail(dns_sock, query, peer);
            continue;
        }
        if !h3_ready {
            // Handshake in progress: queue briefly (the handshake is ~1 RTT).
            if queue.len() >= MAX_QUEUED {
                rt.quality.on_failure();
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
        dispatch_query(
            cfg, dns_sock, quic, h3, pending, req_buf, rt, query, peer, now,
        );
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
    rt: &mut Runtime,
    now: Instant,
) {
    while let Some(q) = queue.pop_front() {
        if q.deadline <= now {
            rt.quality.on_failure();
            send_servfail(dns_sock, &q.query, q.peer);
            continue;
        }
        dispatch_query(
            cfg, dns_sock, quic, h3, pending, req_buf, rt, &q.query, q.peer, now,
        );
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
    rt: &mut Runtime,
    query: &[u8],
    peer: SocketAddr,
    now: Instant,
) {
    let n = query.len();
    let stream = match quic.open_bi() {
        Some(s) => s,
        None => {
            rt.quality.on_failure();
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
        let sep = if cfg.upstream.path.contains('?') {
            '&'
        } else {
            '?'
        };
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
        rt.quality.on_failure();
        send_servfail(dns_sock, query, peer);
        return;
    }
    log::trace!(
        "dispatch stream {stream_idx}: {} request bytes: {:02x?}",
        req_buf.len(),
        &req_buf[..req_buf.len().min(64)]
    );
    h3.register_request(stream_idx);
    pending.insert(
        stream_idx,
        Pending {
            query: query.to_vec(),
            peer,
            deadline: now + cfg.timeout,
            started: now,
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
    rt: &mut Runtime,
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
                rt.fail_streak = 0;
                log::info!(
                    "quic: connected to {} (0-rtt offered: {}, accepted: {})",
                    rt.my_remote,
                    quic.zero_rtt_offered,
                    quic.zero_rtt_accepted
                );
                if !*h3_ready {
                    // Transport parameters arrived: open the control stream
                    // first, then flush queued queries.
                    send_preamble(quic);
                    *h3_ready = true;
                    flush_queue(
                        cfg, dns_sock, quic_sock, quic, h3, pending, queue, req_buf, rt, now,
                    );
                }
            }
            proto::Event::ConnectionLost { reason } => {
                log::warn!("quic: connection lost on {}: {reason}", rt.my_remote);
                // A peer that closes at the application layer is draining us
                // (Cloudflare does this routinely); the path was fine. Counting
                // it as a failure would let server-side churn drive migrations.
                if matches!(&reason, proto::ConnectionError::ApplicationClosed(_)) {
                    rt.fail_streak = 0;
                } else {
                    rt.fail_streak = rt.fail_streak.saturating_add(1);
                }
                fail_all_pending(dns_sock, pending, rt);
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
                        log::trace!(
                            "stream {idx} rx {} bytes: {:02x?}",
                            chunk.len(),
                            &chunk[..chunk.len().min(300)]
                        );
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
                log::trace!(
                    "stream {} stopped by peer, code {:?}",
                    u64::from(id),
                    error_code
                );
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
                    rt.quality
                        .on_success(now.saturating_duration_since(p.started));
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
                    rt.quality.on_failure();
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

/// Periodic tasks: request expiry, score publication, upstream adoption and
/// the weighted migration policy.
#[allow(clippy::too_many_arguments)]
fn housekeeping(
    cfg: &WorkerConfig,
    dns_sock: &UdpSocket,
    quic_sock: &UdpSocket,
    shm: &mut shared::ResolveReader,
    scores: &shared::ScoreWriter,
    quic: &mut Quic,
    h3: &mut H3,
    pending: &mut HashMap<u64, Pending>,
    queue: &mut std::collections::VecDeque<QueuedQuery>,
    rt: &mut Runtime,
    goaway: &mut bool,
    h3_ready: &mut bool,
    reconnect_at: &mut Option<Instant>,
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
            rt.quality.on_failure();
            send_servfail(dns_sock, &p.query, p.peer);
        }
    }

    // ── Adopt the supervisor's latest upstream set (lock-free) ──
    // This only updates the weights the migration policy reads; it never
    // forces a move, so a DNS TTL refresh cannot disturb the hot path.
    if let Some(set) = shm.read_if_changed() {
        if !set.addrs.is_empty() {
            if set.addrs.iter().all(|a| *a != rt.my_remote.ip()) {
                // Our address disappeared from the set: allow an immediate
                // move (checked_sub: Instant arithmetic can overflow early in
                // a process's life).
                rt.last_migrate = now.checked_sub(MIGRATE_MIN_INTERVAL).unwrap_or(now);
            }
            rt.slot = set.slot_for(cfg.child_idx).unwrap_or(rt.slot);
            rt.set = set;
        }
    }

    // ── Expire stale queued (never-sent) queries ──
    while let Some(q) = queue.front() {
        if q.deadline <= now {
            let q = queue.pop_front().unwrap();
            rt.quality.on_failure();
            send_servfail(dns_sock, &q.query, q.peer);
        } else {
            break;
        }
    }

    // ── Publish this worker's measurement for the weight computation ──
    if now.saturating_duration_since(rt.last_score) >= SCORE_INTERVAL {
        let flags = if quic.has_conn() {
            FLAG_CONNECTED
                | if *h3_ready { 0 } else { FLAG_HANDSHAKING }
                | if quic.zero_rtt_offered {
                    FLAG_ZERO_RTT
                } else {
                    0
                }
        } else {
            0
        };
        scores.write(&rt.quality.sample(Some(rt.my_remote.ip()), flags));
        rt.quality.reset_window();
        rt.last_score = now;
    }

    // ── GOAWAY fully drained → reconnect ──
    if *goaway && pending.is_empty() {
        *reconnect_at = Some(now);
    }

    // ── Weighted migration: only when the assignment moved to something
    // meaningfully better than the address we are already on. ──
    if reconnect_at.is_none() {
        if let Some(target) = migration_target(rt, cfg, now) {
            log::info!(
                "migrating {} -> {} (weight {} -> {}, fail_streak {})",
                rt.my_remote,
                target,
                rt.current_weight(),
                rt.set.weight_of(target),
                rt.fail_streak
            );
            rt.my_remote = SocketAddr::new(target, cfg.upstream.port);
            rt.slot = rt
                .set
                .addrs
                .iter()
                .position(|a| *a == target)
                .unwrap_or(rt.slot);
            rt.last_migrate = now;
            rt.fail_streak = 0;
            // The measurement window belongs to the old path.
            rt.quality = Quality::new();
            *reconnect_at = Some(now);
        }
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
            log::info!("quic: (re)connecting to {}", rt.my_remote);
            match connect_and_preamble(quic, now, rt.my_remote, quic_sock) {
                Ok(ready) => {
                    *h3_ready = ready;
                    *reconnect_at = None;
                }
                Err(e) => {
                    log::warn!("reconnect failed: {e}");
                    rt.fail_streak = rt.fail_streak.saturating_add(1);
                    *reconnect_at = Some(now + Duration::from_secs(1));
                }
            }
        }
    }
}

/// Whether worker `idx` of `workers` may start a migration right now.
///
/// Stateless phase: each worker owns one `MIGRATE_STAGGER`-long window per
/// `workers × MIGRATE_STAGGER` cycle. An earlier "time since the weights
/// changed" variant staggered nothing once the revision was old, which let
/// three workers re-handshake in the same second.
fn migration_slot_open(now_secs: u64, workers: usize, idx: usize) -> bool {
    let workers = workers.max(1) as u64;
    let slot = MIGRATE_STAGGER.as_secs().max(1);
    (now_secs / slot) % workers == idx as u64 % workers
}

/// The best alternative to `current`, by published weight.
fn best_alternative(set: &UpstreamSet, current: IpAddr) -> Option<IpAddr> {
    set.addrs
        .iter()
        .zip(set.weights.iter())
        .filter(|(a, w)| **w > 0 && **a != current)
        .max_by_key(|(_, w)| **w)
        .map(|(a, _)| *a)
}

/// Decide whether to move to a different upstream address.
///
/// The weighted assignment says *where this worker should be*; this decides
/// *whether moving is worth a handshake*. Staying put is the default: the hot
/// path is a warm connection, and a migration throws away 0-RTT state and the
/// congestion window. A move needs either a materially better address, or a
/// connection that keeps failing.
fn migration_target(rt: &mut Runtime, cfg: &WorkerConfig, now: Instant) -> Option<IpAddr> {
    let current = rt.my_remote.ip();
    let since_migrate = now.saturating_duration_since(rt.last_migrate);

    // Repeated failures are the one case that justifies an early move — but
    // only to an address that is *known* to be better. With every candidate
    // sitting at the same floor weight, moving is a coin flip between two
    // unmeasured addresses that just burns handshakes.
    if rt.fail_streak >= 2 {
        if since_migrate < FAIL_MIGRATE_INTERVAL {
            return None;
        }
        let current_weight = rt.set.weight_of(current);
        let alt = best_alternative(&rt.set, current)?;
        return (rt.set.weight_of(alt) > current_weight).then_some(alt);
    }

    if since_migrate < MIGRATE_MIN_INTERVAL {
        return None;
    }

    // Stagger so the fleet never re-handshakes all at once.
    if !migration_slot_open(shared::mono_secs(), rt.set.workers as usize, rt.child_idx) {
        return None;
    }

    let slot = rt.set.slot_for(cfg.child_idx)?;
    let target = *rt.set.addrs.get(slot)?;
    if target == current {
        return None;
    }
    // Hysteresis: only move for a materially better address, or when we are
    // on one the supervisor has devalued to zero.
    let current_w = rt.set.weight_of(current);
    let target_w = rt.set.weight_of(target) as u64;
    if current_w == 0 || target_w * 100 >= current_w as u64 * MIGRATE_RATIO_PCT {
        Some(target)
    } else {
        None
    }
}

fn send_servfail(sock: &UdpSocket, query: &[u8], peer: SocketAddr) {
    let mut out = Vec::with_capacity(query.len());
    if dns::build_servfail(query, &mut out) {
        let _ = sock.send_to(&out, peer);
    }
}

/// Fail every in-flight request and count them against the current address.
fn fail_all_pending(sock: &UdpSocket, pending: &mut HashMap<u64, Pending>, rt: &mut Runtime) {
    for (_, p) in pending.drain() {
        rt.quality.on_failure();
        send_servfail(sock, &p.query, p.peer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_slots_rotate_one_worker_at_a_time() {
        let workers = 6usize;
        let slot = MIGRATE_STAGGER.as_secs();
        // Within one cycle exactly one worker is eligible per slot.
        for cycle in 0..3u64 {
            for i in 0..workers {
                let t = cycle * workers as u64 * slot + i as u64 * slot;
                let open: Vec<usize> = (0..workers)
                    .filter(|w| migration_slot_open(t, workers, *w))
                    .collect();
                assert_eq!(open, vec![i], "at t={t} only worker {i} may move");
            }
        }
        // A worker is *not* eligible right after its own slot ends.
        assert!(migration_slot_open(0, workers, 0));
        assert!(!migration_slot_open(MIGRATE_STAGGER.as_secs(), workers, 0));
        // Degenerate worker counts do not divide by zero or wedge anyone.
        assert!(migration_slot_open(7, 0, 0));
        assert!(migration_slot_open(7, 1, 0));
    }
}
