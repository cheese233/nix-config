//! Bootstrap DNS resolution for the DoH upstream hostname.
//!
//! Hand-rolled minimal UDP resolver using `simple-dns` for packet build/parse.
//! Resolves A + AAAA, caches with TTL (stale-while-revalidate).

use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use simple_dns::rdata::{RData, A, AAAA};
use simple_dns::{Name, Packet, Question, CLASS, QTYPE, TYPE};

const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const ATTEMPTS: usize = 3;
/// Fallback TTL if the answer carries none.
const DEFAULT_TTL: u32 = 300;
/// Minimum refresh interval: some resolvers hand out pathologically short
/// TTLs (we saw 1s from dns.google); refreshing that often is wasteful.
const MIN_TTL: u32 = 30;

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("dns packet: {0}")]
    Packet(#[from] simple_dns::SimpleDnsError),
    #[error("no A/AAAA records for {0}")]
    NoRecords(String),
}

/// Cached resolution result with TTL expiry.
#[derive(Clone)]
pub struct ResolveState {
    pub addrs: Vec<IpAddr>,
    pub expires_at: Instant,
}

impl ResolveState {
    #[allow(dead_code)]
    pub fn is_fresh(&self) -> bool {
        self.expires_at > Instant::now()
    }
}

/// Resolve `host` via the bootstrap server, returning the TTL-bounded state
/// plus the upstream socket addresses (IPv6 first — works well with
/// NAT64/DNS64 networks).
pub fn resolve_upstream(
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

/// A minimal synchronous DNS stub resolver pointed at one bootstrap server.
pub struct Bootstrap {
    server: SocketAddr,
}

impl Bootstrap {
    pub fn new(server: IpAddr) -> Self {
        Self {
            server: SocketAddr::new(server, 53),
        }
    }

    /// Resolve `host` to A + AAAA records, returning a TTL-bounded state.
    pub fn resolve(&self, host: &str) -> Result<ResolveState, BootstrapError> {
        // If the host is already an IP literal, skip resolution entirely.
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(ResolveState {
                addrs: vec![ip],
                expires_at: Instant::now() + Duration::from_secs(86400 * 365),
            });
        }

        let mut addrs = Vec::new();
        let mut min_ttl = u32::MAX;
        for qtype in [TYPE::AAAA.into(), TYPE::A.into()] {
            match self.query(host, qtype) {
                Ok((ips, ttl)) => {
                    min_ttl = min_ttl.min(ttl);
                    addrs.extend(ips);
                }
                Err(e) => {
                    log::debug!("bootstrap {qtype:?} query for {host} failed: {e}");
                }
            }
        }
        if addrs.is_empty() {
            return Err(BootstrapError::NoRecords(host.to_string()));
        }
        if min_ttl == u32::MAX || min_ttl == 0 {
            min_ttl = DEFAULT_TTL;
        }
        log::info!("bootstrap resolved {host} → {addrs:?} (ttl={min_ttl}s)");
        Ok(ResolveState {
            addrs,
            expires_at: Instant::now() + Duration::from_secs(min_ttl as u64),
        })
    }

    /// One DNS question round-trip with retries. Returns (ips, min ttl).
    fn query(&self, host: &str, qtype: QTYPE) -> Result<(Vec<IpAddr>, u32), BootstrapError> {
        let id = rand_id();
        let payload = build_query(host, qtype, id)?;
        let sock = bind_matching(&self.server)?;
        sock.set_read_timeout(Some(QUERY_TIMEOUT))?;
        sock.set_write_timeout(Some(QUERY_TIMEOUT))?;

        let mut last_err: Option<io::Error> = None;
        for _ in 0..ATTEMPTS {
            if let Err(e) = sock.send_to(&payload, self.server) {
                last_err = Some(e);
                continue;
            }
            let mut buf = [0u8; 4096];
            match sock.recv_from(&mut buf) {
                Ok((n, _)) => match parse_response(&buf[..n], id).ok_or_else(|| BootstrapError::NoRecords(format!("id mismatch on {id}"))) {
                    Ok(r) => return Ok(r),
                    Err(e) => {
                        log::debug!("bootstrap response parse error: {e}");
                    }
                },
                Err(e) => last_err = Some(e),
            }
        }
        Err(BootstrapError::Io(last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::TimedOut, "no response")
        })))
    }

}

/// Extract matching A/AAAA answers and the minimum TTL.
/// Returns None when the packet's ID doesn't match (not our answer).
fn parse_response(buf: &[u8], want_id: u16) -> Option<(Vec<IpAddr>, u32)> {
    let packet = Packet::parse(buf).ok()?;
    if packet.id() != want_id {
        return None;
    }
    let mut ips = Vec::new();
    let mut min_ttl = u32::MAX;
    for rr in &packet.answers {
        let ip = match &rr.rdata {
            RData::A(A { address }) => Some(IpAddr::from(address.to_be_bytes())),
            RData::AAAA(AAAA { address }) => Some(IpAddr::from(address.to_be_bytes())),
            _ => None,
        };
        if let Some(ip) = ip {
            min_ttl = min_ttl.min(rr.ttl);
            ips.push(ip);
        }
    }
    if min_ttl == u32::MAX {
        min_ttl = DEFAULT_TTL;
    }
    Some((ips, min_ttl))
}


// ---------------------------------------------------------------------------
// Query packet construction (shared by sync and async paths)
// ---------------------------------------------------------------------------

/// Build a single-question DNS query packet for `host`/`qtype` with RD=1.
pub fn build_query(host: &str, qtype: QTYPE, id: u16) -> Result<Vec<u8>, BootstrapError> {
    let mut packet = Packet::new_query(id);
    // RD=1: we want the recursive resolver to recurse for us. Without it
    // the server answers with a bare referral (0 answers).
    packet.set_flags(simple_dns::PacketFlag::RECURSION_DESIRED);
    packet.questions.push(Question::new(
        Name::new(host)?,
        qtype,
        CLASS::IN.into(),
        false,
    ));
    Ok(packet.build_bytes_vec()?)
}

/// Bind a UDP socket matching the bootstrap server's address family.
fn bind_matching(server: &SocketAddr) -> io::Result<UdpSocket> {
    // NOTE: bind via a concrete SocketAddr — never through ToSocketAddrs,
    // which would call getaddrinfo and can hang on nss-mdns systems.
    let any: SocketAddr = if server.is_ipv6() {
        SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 0)
    } else {
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0)
    };
    UdpSocket::bind(any)
}

// ---------------------------------------------------------------------------
// AsyncResolve — non-blocking bootstrap resolution for the supervisor loop
// ---------------------------------------------------------------------------

/// Non-blocking A+AAAA resolver: both queries are sent at once; responses
/// are matched by ID. Drive via `on_readable` / `on_timeout` from a poll loop.
pub struct AsyncResolve {
    sock: UdpSocket,
    server: SocketAddr,
    q_aaaa: Vec<u8>,
    q_a: Vec<u8>,
    id_aaaa: u16,
    id_a: u16,
    deadline: Instant,
    attempts: usize,
    aaaa: Option<(Vec<IpAddr>, u32)>,
    a: Option<(Vec<IpAddr>, u32)>,
}

impl AsyncResolve {
    /// Send both queries. `now` seeds the first round's deadline.
    pub fn start(server: SocketAddr, host: &str, now: Instant) -> Result<Self, BootstrapError> {
        let id_aaaa = rand_id();
        let id_a = rand_id().wrapping_add(1);
        let q_aaaa = build_query(host, TYPE::AAAA.into(), id_aaaa)?;
        let q_a = build_query(host, TYPE::A.into(), id_a)?;
        let sock = bind_matching(&server)?;
        sock.set_nonblocking(true)?;
        let this = Self {
            sock,
            server,
            q_aaaa,
            q_a,
            id_aaaa,
            id_a,
            deadline: now + QUERY_TIMEOUT,
            attempts: 1,
            aaaa: None,
            a: None,
        };
        this.send_both();
        Ok(this)
    }

    fn send_both(&self) {
        let _ = self.sock.send_to(&self.q_aaaa, self.server);
        let _ = self.sock.send_to(&self.q_a, self.server);
    }

    /// The fd to poll for readability.
    pub fn socket(&self) -> &UdpSocket {
        &self.sock
    }

    /// Current round's deadline.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Socket is readable: drain responses. Returns Some(result) when the
    /// resolution is complete (or failed terminally).
    pub fn on_readable(&mut self) -> Option<Result<(Vec<IpAddr>, u32), BootstrapError>> {
        let mut buf = [0u8; 4096];
        loop {
            match self.sock.recv_from(&mut buf) {
                Ok((n, _)) => {
                    if let Some((ips, ttl)) = parse_response(&buf[..n], self.id_aaaa) {
                        self.aaaa = Some((ips, ttl));
                    } else if let Some((ips, ttl)) = parse_response(&buf[..n], self.id_a) {
                        self.a = Some((ips, ttl));
                    }
                    // ID mismatch / parse error: ignore the datagram.
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        if self.aaaa.is_some() && self.a.is_some() {
            Some(Ok(self.finish()))
        } else {
            None
        }
    }

    /// Deadline passed: resend (bounded attempts) or fail terminally.
    pub fn on_timeout(&mut self, now: Instant) -> Option<BootstrapError> {
        if self.attempts >= ATTEMPTS {
            return Some(BootstrapError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "bootstrap: all attempts timed out",
            )));
        }
        self.attempts += 1;
        self.deadline = now + QUERY_TIMEOUT;
        self.send_both();
        None
    }

    /// Merge both answers: (ips, min ttl).
    fn finish(&self) -> (Vec<IpAddr>, u32) {
        let (v6, t6) = self.aaaa.clone().unwrap_or_default();
        let (v4, t4) = self.a.clone().unwrap_or_default();
        let mut addrs = v6;
        addrs.extend(v4);
        let ttl = match (t6, t4) {
            (0, 0) => DEFAULT_TTL,
            (a, 0) => a,
            (0, b) => b,
            (a, b) => a.min(b),
        };
        (addrs, ttl.max(MIN_TTL))
    }
}

/// Cheap per-process random id (no crypto needed for bootstrap on localhost).
fn rand_id() -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos as u16) ^ ((nanos >> 16) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_literal_passthrough() {
        let b = Bootstrap::new(IpAddr::from([127, 0, 0, 1]));
        let st = b.resolve("8.8.8.8").unwrap();
        assert_eq!(st.addrs, vec![IpAddr::from([8, 8, 8, 8])]);
        let st = b.resolve("2001:4860:4860::8888").unwrap();
        assert_eq!(
            st.addrs,
            vec!["2001:4860:4860::8888".parse::<IpAddr>().unwrap()]
        );
    }

    #[test]
    fn parse_response_extracts_a_records() {
        // Hand-built response: id 0x1234, QR=1, QDCOUNT=1, ANCOUNT=2,
        // question "x.com A IN", answers 1.2.3.4 (ttl 60) and 5.6.7.8 (ttl 30).
        let mut buf: Vec<u8> = vec![
            0x12, 0x34, 0x81, 0x80, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00,
        ];
        buf.push(1);
        buf.extend_from_slice(b"x");
        buf.push(3);
        buf.extend_from_slice(b"com");
        buf.push(0);
        buf.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // A IN
        // Answer 1: pointer to name, A, IN, ttl 60, rdlen 4, 1.2.3.4
        buf.extend_from_slice(&[0xC0, 0x0C, 0x00, 0x01, 0x00, 0x01]);
        buf.extend_from_slice(&60u32.to_be_bytes());
        buf.extend_from_slice(&[0x00, 0x04, 1, 2, 3, 4]);
        // Answer 2: 5.6.7.8 ttl 30
        buf.extend_from_slice(&[0xC0, 0x0C, 0x00, 0x01, 0x00, 0x01]);
        buf.extend_from_slice(&30u32.to_be_bytes());
        buf.extend_from_slice(&[0x00, 0x04, 5, 6, 7, 8]);

        let b = Bootstrap::new(IpAddr::from([127, 0, 0, 1]));
        let (ips, ttl) = parse_response(&buf, 0x1234).unwrap();
        assert_eq!(
            ips,
            vec![
                IpAddr::from([1, 2, 3, 4]),
                IpAddr::from([5, 6, 7, 8])
            ]
        );
        assert_eq!(ttl, 30);
    }

    #[test]
    fn parse_response_id_mismatch() {
        let buf = [
            0x00, 0x01, 0x81, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let b = Bootstrap::new(IpAddr::from([127, 0, 0, 1]));
        assert!(parse_response(&buf, 0x1234).is_none());
    }

    #[test]
    fn query_builds_valid_packet() {
        // Verify simple-dns builds a parseable query (round-trip through parse).
        let mut packet = Packet::new_query(0xABCD);
        packet.questions.push(Question::new(
            Name::new("dns.google").unwrap(),
            TYPE::A.into(),
            CLASS::IN.into(),
            false,
        ));
        let bytes = packet.build_bytes_vec().unwrap();
        let parsed = Packet::parse(&bytes).unwrap();
        assert_eq!(parsed.id(), 0xABCD);
        assert_eq!(parsed.questions.len(), 1);
    }

    // ── AsyncResolve tests with a loopback stub DNS server ──

    use std::net::UdpSocket;
    use std::sync::mpsc;

    /// A minimal stub DNS server: receives queries, hands them to the test
    /// via a channel, and sends back whatever the test gives it.
    struct StubDns {
        addr: SocketAddr,
        rx: mpsc::Receiver<(Vec<u8>, SocketAddr)>,
        sock: UdpSocket,
    }

    impl StubDns {
        fn spawn() -> Self {
            let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
            let addr = sock.local_addr().unwrap();
            let sock2 = sock.try_clone().unwrap();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match sock2.recv_from(&mut buf) {
                        Ok((n, peer)) => {
                            if tx.send((buf[..n].to_vec(), peer)).is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    }
                }
            });
            Self { addr, rx, sock }
        }

        fn recv_query(&self) -> (Vec<u8>, SocketAddr) {
            self.rx
                .recv_timeout(Duration::from_secs(2))
                .expect("stub: no query received")
        }

        fn reply_a(&self, query: &[u8], peer: SocketAddr, ip: [u8; 4], ttl: u32) {
            let resp = build_a_response(query, ip, ttl);
            self.sock.send_to(&resp, peer).unwrap();
        }

        fn reply_aaaa(&self, query: &[u8], peer: SocketAddr, ip: [u8; 16], ttl: u32) {
            let resp = build_aaaa_response(query, ip, ttl);
            self.sock.send_to(&resp, peer).unwrap();
        }
    }

    /// Build a DNS response echoing the question, with one A answer.
    fn build_a_response(query: &[u8], ip: [u8; 4], ttl: u32) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&query[0..2]); // ID
        r.extend_from_slice(&[0x81, 0x80]); // QR RD RA
        r.extend_from_slice(&[0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]);
        r.extend_from_slice(&query[12..]); // question
        r.extend_from_slice(&[0xC0, 0x0C, 0x00, 0x01, 0x00, 0x01]);
        r.extend_from_slice(&ttl.to_be_bytes());
        r.extend_from_slice(&[0x00, 0x04]);
        r.extend_from_slice(&ip);
        r
    }

    fn build_aaaa_response(query: &[u8], ip: [u8; 16], ttl: u32) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&query[0..2]);
        r.extend_from_slice(&[0x81, 0x80]);
        r.extend_from_slice(&[0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]);
        r.extend_from_slice(&query[12..]);
        r.extend_from_slice(&[0xC0, 0x0C, 0x00, 0x1C, 0x00, 0x01]);
        r.extend_from_slice(&ttl.to_be_bytes());
        r.extend_from_slice(&[0x00, 0x10]);
        r.extend_from_slice(&ip);
        r
    }

    #[test]
    fn async_resolve_out_of_order_replies() {
        let stub = StubDns::spawn();
        let now = Instant::now();
        let mut r = AsyncResolve::start(stub.addr, "x.test", now).unwrap();

        // Both queries arrive (AAAA + A, order unspecified). Reply to the
        // A query first (out of order vs. send order).
        let (q1, p1) = stub.recv_query();
        let (q2, p2) = stub.recv_query();
        let qtype_of = |q: &[u8]| u16::from_be_bytes([q[q.len() - 4], q[q.len() - 3]]);
        let ((a_q, a_p), (aaaa_q, aaaa_p)) = if qtype_of(&q1) == 1 {
            ((q1, p1), (q2, p2))
        } else {
            ((q2, p2), (q1, p1))
        };
        assert_eq!(qtype_of(&a_q), 1);
        assert_eq!(qtype_of(&aaaa_q), 28);

        stub.reply_a(&a_q, a_p, [1, 2, 3, 4], 60);
        // Not complete yet — AAAA still missing.
        assert!(r.on_readable().is_none());
        stub.reply_aaaa(
            &aaaa_q,
            aaaa_p,
            [0x20, 1, 0x48, 0x60, 0x48, 0x60, 0, 0, 0, 0, 0, 0, 0, 0, 0x88, 0x88],
            30,
        );
        let (ips, ttl) = r.on_readable().expect("should complete").unwrap();
        assert_eq!(ips.len(), 2);
        assert_eq!(ttl, 30);
        assert!(ips.contains(&IpAddr::from([1, 2, 3, 4])));
        assert!(ips.contains(&"2001:4860:4860::8888".parse().unwrap()));
    }

    #[test]
    fn async_resolve_clamps_pathological_ttl() {
        let stub = StubDns::spawn();
        let now = Instant::now();
        let mut r = AsyncResolve::start(stub.addr, "x.test", now).unwrap();
        let (q1, p1) = stub.recv_query();
        let (q2, p2) = stub.recv_query();
        let qtype_of = |q: &[u8]| u16::from_be_bytes([q[q.len() - 4], q[q.len() - 3]]);
        let ((a_q, a_p), (aaaa_q, aaaa_p)) = if qtype_of(&q1) == 1 {
            ((q1, p1), (q2, p2))
        } else {
            ((q2, p2), (q1, p1))
        };
        // TTL=1 (pathological) and TTL=118 → clamped to MIN_TTL (30).
        stub.reply_a(&a_q, a_p, [1, 2, 3, 4], 1);
        stub.reply_aaaa(&aaaa_q, aaaa_p, [0u8; 16], 118);
        let (_ips, ttl) = r.on_readable().expect("should complete").unwrap();
        assert_eq!(ttl, MIN_TTL);
    }

    #[test]
    fn async_resolve_ignores_mismatched_id() {
        let stub = StubDns::spawn();
        let now = Instant::now();
        let mut r = AsyncResolve::start(stub.addr, "x.test", now).unwrap();
        let (q1, p1) = stub.recv_query();
        let (_q2, _p2) = stub.recv_query();

        // Reply with a wrong ID: must be ignored (no completion).
        let mut bad = build_a_response(&q1, [5, 6, 7, 8], 60);
        bad[0] ^= 0xFF;
        stub.sock.send_to(&bad, p1).unwrap();
        assert!(r.on_readable().is_none());
    }

    #[test]
    fn async_resolve_timeout_retries_then_fails() {
        let stub = StubDns::spawn();
        let now = Instant::now();
        let mut r = AsyncResolve::start(stub.addr, "x.test", now).unwrap();
        let _ = stub.recv_query();
        let _ = stub.recv_query();

        let now2 = now + Duration::from_secs(4);
        // First timeout: resend (no terminal error).
        assert!(r.on_timeout(now2).is_none());
        // The resend should arrive at the stub.
        let _ = stub.recv_query();
        let _ = stub.recv_query();
        // Second timeout: another resend, still not terminal.
        let now3 = now2 + Duration::from_secs(4);
        assert!(r.on_timeout(now3).is_none());
        // Third timeout: attempts exhausted → terminal error.
        let now4 = now3 + Duration::from_secs(4);
        assert!(r.on_timeout(now4).is_some());
    }
}
