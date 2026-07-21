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
        let payload = packet.build_bytes_vec()?;

        // NOTE: bind via a concrete SocketAddr — never through ToSocketAddrs,
        // which would call getaddrinfo and can hang on nss-mdns systems.
        // Match the socket family to the bootstrap server's family.
        let any: SocketAddr = if self.server.is_ipv6() {
            SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 0)
        } else {
            SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0)
        };
        let sock = UdpSocket::bind(any)?;
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
                Ok((n, _)) => match self.parse_response(&buf[..n], id) {
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

    /// Extract matching A/AAAA answers and the minimum TTL.
    fn parse_response(
        &self,
        buf: &[u8],
        want_id: u16,
    ) -> Result<(Vec<IpAddr>, u32), BootstrapError> {
        let packet = Packet::parse(buf)?;
        if packet.id() != want_id {
            return Err(BootstrapError::NoRecords(format!(
                "id mismatch: got {}, want {want_id}",
                packet.id()
            )));
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
        Ok((ips, min_ttl))
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
        let (ips, ttl) = b.parse_response(&buf, 0x1234).unwrap();
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
        assert!(b.parse_response(&buf, 0x1234).is_err());
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
}
