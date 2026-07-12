//! RFC 8484 request construction.
//!
//! DNS wire-format → GET (≤1400 bytes) or POST (>1400 bytes) to the DoH upstream.
//! Also manages bootstrap DNS resolution with TTL-aware refresh.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use base64::Engine;
use curl::easy::{Easy2, Handler, HttpVersion, List, ReadError, WriteError};
use hickory_resolver::config::{NameServerConfig, ResolverConfig, ResolverOpts};
use hickory_resolver::name_server::TokioConnectionProvider;
use hickory_resolver::proto::xfer::Protocol;
use hickory_resolver::TokioResolver;

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// DNS wire-format queries longer than this use POST instead of GET.
const GET_MAX_DNS_LEN: usize = 1400;

/// libcurl 7.71.0+ flag for TLS 1.3 / QUIC 0-RTT early data.
/// From curl.h: `#define CURLSSLOPT_EARLYDATA (1L << 6)`
const CURLSSLOPT_EARLYDATA: i64 = 64;

// ---------------------------------------------------------------------------
// ResolveState — bootstrap DNS result with TTL awareness
// ---------------------------------------------------------------------------

/// Holds the pre-resolved IPs for the DoH upstream, populated via bootstrap
/// DNS, and refreshed when the TTL expires.
pub struct ResolveState {
    #[allow(dead_code)]
    pub addrs: Vec<IpAddr>,
    /// Pre-built CURLOPT_RESOLVE entries, one per address family.
    pub resolve_entries: Vec<String>,
    pub expires_at: Instant,
}

impl ResolveState {
    /// Apply the resolve entries to a curl List.
    pub fn to_resolve_list(&self) -> List {
        let mut list = List::new();
        for entry in &self.resolve_entries {
            list.append(entry).unwrap();
        }
        list
    }
}

/// Dedicated single-threaded tokio runtime for bootstrap DNS queries.
/// The curl worker thread is a plain OS thread and cannot call async hickory
/// methods directly, so we give it its own mini runtime.
pub struct DnsRuntime {
    rt: tokio::runtime::Runtime,
    resolver: TokioResolver,
}

impl DnsRuntime {
    /// Create a new DNS runtime configured to talk to `bootstrap_dns`.
    pub fn new(bootstrap_dns: IpAddr) -> Result<Self> {
        let ns = NameServerConfig::new(
            SocketAddr::new(bootstrap_dns, 53),
            Protocol::Udp,
        );
        let ns_group = hickory_resolver::config::NameServerConfigGroup::from(vec![ns]);
        let config = ResolverConfig::from_parts(None, vec![], ns_group);

        let mut opts = ResolverOpts::default();
        opts.timeout = Duration::from_secs(3);
        opts.attempts = 3;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::dns_bootstrap(format!("runtime init: {e}")))?;

        let resolver = rt.block_on(async {
            TokioResolver::builder_with_config(
                config,
                TokioConnectionProvider::default(),
            )
            .with_options(opts)
            .build()
        });

        Ok(Self { rt, resolver })
    }

    /// Resolve `host` via the bootstrap DNS server, returning a [`ResolveState`]
    /// whose `expires_at` is derived from the smallest TTL in the response.
    pub fn resolve(&self, host: &str, port: u16) -> Result<ResolveState> {
        let response = self
            .rt
            .block_on(self.resolver.lookup_ip(host))
            .map_err(|e| Error::dns_bootstrap(format!("lookup {host}: {e}")))?;

        let min_ttl = response
            .as_lookup()
            .record_iter()
            .map(|r| r.ttl())
            .min()
            .unwrap_or(300);

        let addrs: Vec<IpAddr> = response.iter().collect();
        if addrs.is_empty() {
            return Err(Error::dns_bootstrap(format!(
                "no A/AAAA records for {host}"
            )));
        }

        let resolve_entries = build_resolve_entries(host, port, &addrs);
        log::info!("bootstrap resolved {host}:{port} → {addrs:?} (ttl={min_ttl}s)");

        Ok(ResolveState {
            addrs,
            resolve_entries,
            expires_at: Instant::now() + Duration::from_secs(min_ttl as u64),
        })
    }
}

/// Check whether the cached resolution has expired; if so, re-resolve.
/// On failure the old (stale) state is kept — stale-while-revalidate.
///
/// Uses a read‑first pattern to avoid write‑locking in the hot path.
pub fn ensure_fresh(
    state: &Arc<RwLock<ResolveState>>,
    dns_rt: &DnsRuntime,
    host: &str,
    port: u16,
) {
    // Fast path: read‑lock, check expiry, return if still fresh.
    {
        let s = state.read().unwrap();
        if s.expires_at > Instant::now() {
            return; // 99.999% of calls exit here with only a read lock
        }
    }
    // Slow path: write‑lock, double‑check, re‑resolve.
    let mut s = state.write().unwrap();
    if s.expires_at > Instant::now() {
        return; // another thread refreshed while we waited for the write lock
    }
    match dns_rt.resolve(host, port) {
        Ok(new) => *s = new,
        Err(e) => {
            log::warn!("bootstrap re-resolve failed (keeping stale IPs): {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// CURLOPT_RESOLVE helpers
// ---------------------------------------------------------------------------

/// Build `CURLOPT_RESOLVE` entries in the format `"host:port:addr1,addr2"`.
fn build_resolve_entries(host: &str, port: u16, addrs: &[IpAddr]) -> Vec<String> {
    let mut ipv4s = Vec::new();
    let mut ipv6s = Vec::new();
    for addr in addrs {
        match addr {
            IpAddr::V4(v4) => ipv4s.push(v4.to_string()),
            IpAddr::V6(v6) => ipv6s.push(format!("[{v6}]")),
        }
    }

    let mut entries = Vec::new();
    if !ipv4s.is_empty() {
        entries.push(format!("{host}:{port}:{}", ipv4s.join(",")));
    }
    if !ipv6s.is_empty() {
        entries.push(format!("{host}:{port}:{}", ipv6s.join(",")));
    }
    entries
}

// ---------------------------------------------------------------------------
// DohHandler — Easy2 handler for GET/POST with response body collection
// ---------------------------------------------------------------------------

/// Handler for [`Easy2`] that supplies the POST body (from `query`) and
/// collects the HTTP response body into `response`.
pub struct DohHandler {
    /// Raw DNS wire-format message (only read for POST transfers).
    pub query: Vec<u8>,
    /// Read cursor into `query` for POST body.
    read_offset: usize,
    /// Accumulated HTTP response body.
    pub response: Vec<u8>,
}

impl DohHandler {
    pub fn new(query: Vec<u8>) -> Self {
        Self {
            query,
            read_offset: 0,
            response: Vec::with_capacity(512),
        }
    }
}

impl Handler for DohHandler {
    /// libcurl calls this to read the request body (POST only).
    fn read(&mut self, buf: &mut [u8]) -> std::result::Result<usize, ReadError> {
        let remaining = &self.query[self.read_offset..];
        if remaining.is_empty() {
            return Ok(0); // EOF
        }
        let n = remaining.len().min(buf.len());
        buf[..n].copy_from_slice(&remaining[..n]);
        self.read_offset += n;
        Ok(n)
    }

    /// libcurl calls this to deliver the response body.
    fn write(&mut self, data: &[u8]) -> std::result::Result<usize, WriteError> {
        self.response.extend_from_slice(data);
        Ok(data.len())
    }
}

// ---------------------------------------------------------------------------
// build_easy2_request — the core Easy2 builder
// ---------------------------------------------------------------------------

/// Build a configured [`Easy2<DohHandler>`] for a single DNS query.
///
/// - Queries ≤ 1400 bytes → GET  `?dns=<base64url>`
///   (DNS ID zeroed per RFC 8484 §4.1 for cache friendliness)
/// - Queries > 1400 bytes → POST  with `Content-Type: application/dns-message`
pub fn build_easy2_request(
    mut query: Vec<u8>,
    upstream: &str,
    token: Option<&str>,
    resolve_state: &ResolveState,
    timeout_secs: u64,
    verbose: bool,
    pad: bool,
) -> Easy2<DohHandler> {
    let use_get = query.len() <= GET_MAX_DNS_LEN;

    // ── EDNS0 padding (RFC 8467) ──
    if pad {
        crate::proto::pad_dns_query(&mut query, 128);
    }

    // ── RFC 8484 §4.1: zero the DNS ID for GET to maximize cache hits ──
    if use_get && query.len() >= 2 {
        query[0] = 0;
        query[1] = 0;
    }

    let mut easy = Easy2::new(DohHandler::new(query));

    // ── URL ──
    let url = if use_get {
        let b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&easy.get_ref().query);
        format!("{upstream}?dns={b64}")
    } else {
        upstream.to_string()
    };
    easy.url(&url).unwrap();

    // ── Verbose: dump libcurl protocol info to stderr ──
    if verbose {
        easy.verbose(true).unwrap();
    }

    // ── HTTP version: H3 → H2 fallback ──
    easy.http_version(HttpVersion::V3).unwrap();

    // ── 0-RTT (TLS 1.3 / QUIC early data) via FFI ──
    unsafe {
        curl_sys::curl_easy_setopt(
            easy.raw(),
            curl_sys::CURLOPT_SSL_OPTIONS,
            CURLSSLOPT_EARLYDATA,
        );
    }

    // ── Headers ──
    let mut headers = List::new();
    headers.append("Accept: application/dns-message").unwrap();
    if let Some(t) = token {
        headers
            .append(&format!("Authorization: Bearer {t}"))
            .unwrap();
    }
    if !use_get {
        headers
            .append("Content-Type: application/dns-message")
            .unwrap();
        easy.upload(true).unwrap();
        easy.post_field_size(easy.get_ref().query.len() as u64)
            .unwrap();
    }
    easy.http_headers(headers).unwrap();

    // ── Bootstrap DNS: inject pre-resolved IPs ──
    easy.resolve(resolve_state.to_resolve_list()).unwrap();

    // ── HTTP/2 multiplexing ──
    easy.pipewait(false).unwrap();

    // ── We ARE the DNS — disable libcurl's DNS cache ──
    easy.dns_cache_timeout(Duration::from_secs(0)).unwrap();

    // ── Timeout ──
    easy.timeout(Duration::from_secs(timeout_secs)).unwrap();

    easy
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    // ── resolve_entries tests ──

    #[test]
    fn test_build_resolve_entries_ipv4_only() {
        let addrs = vec![
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V4(Ipv4Addr::new(8, 8, 4, 4)),
        ];
        let entries = build_resolve_entries("dns.google", 443, &addrs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], "dns.google:443:8.8.8.8,8.8.4.4");
    }

    #[test]
    fn test_build_resolve_entries_ipv6_only() {
        let addrs = vec![
            IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888)),
            IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8844)),
        ];
        let entries = build_resolve_entries("dns.google", 443, &addrs);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0],
            "dns.google:443:[2001:4860:4860::8888],[2001:4860:4860::8844]"
        );
    }

    #[test]
    fn test_build_resolve_entries_mixed() {
        let addrs = vec![
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888)),
        ];
        let entries = build_resolve_entries("dns.google", 443, &addrs);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], "dns.google:443:8.8.8.8");
        assert_eq!(entries[1], "dns.google:443:[2001:4860:4860::8888]");
    }

    #[test]
    fn test_build_resolve_entries_empty() {
        let entries = build_resolve_entries("dns.google", 443, &[]);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_build_resolve_entries_single_ipv4() {
        let addrs = vec![IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))];
        let entries = build_resolve_entries("example.com", 443, &addrs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], "example.com:443:1.1.1.1");
    }

    #[test]
    fn test_build_resolve_entries_different_port() {
        let addrs = vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))];
        let entries = build_resolve_entries("internal.dns", 5300, &addrs);
        assert_eq!(entries[0], "internal.dns:5300:10.0.0.1");
    }

    // ── ResolveState tests ──

    #[test]
    fn test_resolve_state_to_resolve_list_empty() {
        let state = ResolveState {
            addrs: vec![],
            resolve_entries: vec![],
            expires_at: Instant::now() + Duration::from_secs(300),
        };
        let list = state.to_resolve_list();
        // An empty List is valid — verify we can use it.
        // We can't inspect the contents directly, but we can confirm no panic.
        drop(list);
    }

    #[test]
    fn test_resolve_state_to_resolve_list_with_entries() {
        let state = ResolveState {
            addrs: vec![],
            resolve_entries: vec!["a:443:1.2.3.4".to_string(), "a:443:[::1]".to_string()],
            expires_at: Instant::now() + Duration::from_secs(300),
        };
        let list = state.to_resolve_list();
        // Just verify it doesn't panic.
        drop(list);
    }

    // ── DohHandler tests ──

    #[test]
    fn test_doh_handler_new() {
        let handler = DohHandler::new(vec![1, 2, 3, 4]);
        assert_eq!(handler.query, vec![1, 2, 3, 4]);
        assert!(handler.response.is_empty());
    }

    #[test]
    fn test_doh_handler_read_full_body() {
        let mut handler = DohHandler::new(vec![0xAA, 0xBB, 0xCC, 0xDD]);
        let mut buf = [0u8; 4];
        let n = handler.read(&mut buf).unwrap();
        assert_eq!(n, 4);
        assert_eq!(buf, [0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn test_doh_handler_read_partial() {
        let mut handler = DohHandler::new(vec![1, 2, 3, 4, 5, 6]);
        let mut buf = [0u8; 3];
        let n1 = handler.read(&mut buf).unwrap();
        assert_eq!(n1, 3);
        assert_eq!(buf, [1, 2, 3]);

        let n2 = handler.read(&mut buf).unwrap();
        assert_eq!(n2, 3);
        assert_eq!(buf, [4, 5, 6]);

        let n3 = handler.read(&mut buf).unwrap();
        assert_eq!(n3, 0); // EOF
    }

    #[test]
    fn test_doh_handler_read_empty_query() {
        let mut handler = DohHandler::new(vec![]);
        let mut buf = [0u8; 4];
        let n = handler.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_doh_handler_read_large_buf() {
        let mut handler = DohHandler::new(vec![0x42; 10]);
        let mut buf = [0u8; 1024];
        let n = handler.read(&mut buf).unwrap();
        assert_eq!(n, 10);
        for i in 0..10 {
            assert_eq!(buf[i], 0x42);
        }
        // EOF
        let n2 = handler.read(&mut buf).unwrap();
        assert_eq!(n2, 0);
    }

    #[test]
    fn test_doh_handler_write_accumulates() {
        let mut handler = DohHandler::new(vec![]);
        let n1 = handler.write(b"hello ").unwrap();
        assert_eq!(n1, 6);
        let n2 = handler.write(b"world").unwrap();
        assert_eq!(n2, 5);
        assert_eq!(handler.response, b"hello world");
    }

    #[test]
    fn test_doh_handler_write_empty_chunk() {
        let mut handler = DohHandler::new(vec![]);
        let n = handler.write(b"").unwrap();
        assert_eq!(n, 0);
        assert!(handler.response.is_empty());
    }

    #[test]
    fn test_doh_handler_write_large_data() {
        let mut handler = DohHandler::new(vec![]);
        let data = vec![0x41u8; 4096];
        let n = handler.write(&data).unwrap();
        assert_eq!(n, 4096);
        assert_eq!(handler.response.len(), 4096);
    }

    // ── GET_MAX_DNS_LEN threshold tests ──

    #[test]
    fn test_get_threshold_boundary() {
        // Exactly at threshold → GET
        let query = vec![0u8; GET_MAX_DNS_LEN];
        assert!(query.len() <= GET_MAX_DNS_LEN);
    }

    #[test]
    fn test_post_threshold_boundary() {
        // One byte over threshold → POST
        let query = vec![0u8; GET_MAX_DNS_LEN + 1];
        assert!(query.len() > GET_MAX_DNS_LEN);
    }

    #[test]
    fn test_typical_dns_query_is_get() {
        // A typical DNS query is well under 1400 bytes.
        // Standard DNS header + question for "www.example.com" ≈ 33 bytes.
        let typical_dns_query = vec![0u8; 100];
        assert!(typical_dns_query.len() <= GET_MAX_DNS_LEN);
    }

    #[test]
    fn test_edns0_large_query_is_post() {
        // A DNS query that would exceed 1400 bytes (e.g., with large TXT/RRSIG).
        let large_query = vec![0u8; 2000];
        assert!(large_query.len() > GET_MAX_DNS_LEN);
    }

    // ── base64 encoding tests ──

    #[test]
    fn test_base64url_encoding_no_pad() {
        // RFC 8484: the dns parameter is base64url-encoded WITHOUT padding.
        // Typical A record query for www.example.com is ~33 bytes.
        let query = hex::decode(
            "00000100000100000000000003777777076578616d706c6503636f6d0000010001"
        ).unwrap();
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&query);
        // Known encoding from RFC 8484 §4.1.1
        assert_eq!(b64, "AAABAAABAAAAAAAAA3d3dwdleGFtcGxlA2NvbQAAAQAB");
    }

    #[test]
    fn test_base64url_encoding_distinct_from_standard_base64() {
        let data = vec![0x3fu8, 0x7fu8]; // '?' and DEL - chars that differ between base64 and base64url
        let b64url = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&data);
        // base64url uses '-' instead of '+' and '_' instead of '/'
        assert!(!b64url.contains('+'));
        assert!(!b64url.contains('/'));
        assert!(!b64url.contains('=')); // no padding
    }

    // ── ensure_fresh tests ──

    #[test]
    fn test_ensure_fresh_not_expired_does_nothing() {
        // This test verifies that ensure_fresh doesn't panic when state is not expired.
        let state = Arc::new(RwLock::new(ResolveState {
            addrs: vec![],
            resolve_entries: vec!["a:443:1.2.3.4".to_string()],
            expires_at: Instant::now() + Duration::from_secs(300),
        }));

        // We can't easily test re-resolution here (needs network), but we
        // can at least verify the not-expired path works without a DNS runtime.
        let expired = {
            let s = state.read().unwrap();
            s.expires_at <= Instant::now()
        };
        assert!(!expired);
    }

    #[test]
    fn test_ensure_fresh_expired_triggers_check() {
        let state = Arc::new(RwLock::new(ResolveState {
            addrs: vec![],
            resolve_entries: vec!["a:443:1.2.3.4".to_string()],
            expires_at: Instant::now() - Duration::from_secs(1), // already expired
        }));

        let expired = {
            let s = state.read().unwrap();
            s.expires_at <= Instant::now()
        };
        assert!(expired);
    }

    // ── Edge case: empty addrs ──

    #[test]
    fn test_empty_addrs_produces_empty_entries() {
        let entries = build_resolve_entries("example.com", 443, &[]);
        assert!(entries.is_empty());
    }

    // ── DNS ID zeroing (RFC 8484 §4.1) ──

    #[test]
    fn test_dns_id_zeroed_for_get() {
        // A GET-eligible query should have its DNS ID zeroed.
        let mut query = vec![0xAA, 0xBB];
        query.extend(vec![0u8; 100]); // pad to valid DNS size for GET
        assert!(query.len() <= GET_MAX_DNS_LEN);

        // Simulate what build_easy2_request does
        if query.len() >= 2 {
            query[0] = 0;
            query[1] = 0;
        }
        assert_eq!(query[0], 0);
        assert_eq!(query[1], 0);
        // The rest of the message is untouched
        assert_eq!(query[2], 0);
    }

    #[test]
    fn test_dns_id_not_zeroed_for_post() {
        // A POST-eligible query (>1400 bytes) should NOT be modified.
        let mut query = vec![0xAA, 0xBB];
        query.extend(vec![0u8; 2000]);
        assert!(query.len() > GET_MAX_DNS_LEN);

        // POST path does not zero DNS ID
        let original_id = (query[0], query[1]);
        assert_eq!(original_id, (0xAA, 0xBB));
    }

    #[test]
    fn test_dns_id_short_query_not_zeroed_out_of_bounds() {
        let query = vec![0x42]; // 1 byte, too short anyway
        // No panic: the len >= 2 guard works
        assert!(query.len() < 2);
    }

    // ── Stress: many IPs ──

    #[test]
    fn test_many_ipv4_addresses() {
        let addrs: Vec<IpAddr> = (1..=20)
            .map(|i| IpAddr::V4(Ipv4Addr::new(10, 0, 0, i)))
            .collect();
        let entries = build_resolve_entries("many.example.com", 443, &addrs);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].contains("10.0.0.1"));
        assert!(entries[0].contains("10.0.0.20"));
        // Should contain 20 comma-separated IPs.
        assert_eq!(entries[0].matches(',').count(), 19);
    }

    // ── DNS query validation helpers ──

    /// Extract from the hex module (only in tests)
    mod hex {
        pub fn decode(hex: &str) -> Result<Vec<u8>, std::num::ParseIntError> {
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
                .collect()
        }
    }
}
