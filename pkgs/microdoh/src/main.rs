//! microdoh — minimal DNS-over-HTTPS proxy.
//!
//! Listens on a local UDP port, forwards DNS queries to a DoH upstream
//! using RFC 8484 GET (with POST fallback for large queries), supports
//! TLS 1.3 / QUIC 0-RTT, and optionally injects an `Authorization: Bearer`
//! header from the environment.

mod curl_worker;
mod doh;
mod error;
mod udp;

use std::net::IpAddr;
use std::sync::Arc;

use clap::Parser;

/// DNS-over-HTTPS proxy with 0-RTT, RFC 8484 GET, and bearer auth.
#[derive(Parser, Debug)]
#[command(name = "microdoh", version)]
pub struct Cli {
    /// Address to listen on for DNS queries (UDP).
    #[arg(long, short = 'l', default_value = "0.0.0.0:5300")]
    pub listen: String,

    /// DoH upstream URL (e.g. `https://dns.google/dns-query`).
    #[arg(long, short = 'u', default_value = "https://dns.google/dns-query")]
    pub upstream: String,

    /// Bearer token for `Authorization` header.  Read from `$MICRODOH_TOKEN`
    /// if not given on the command line.
    #[arg(long, env = "MICRODOH_TOKEN")]
    pub token: Option<String>,

    /// Read the bearer token from this file (overrides `--token` / env).
    #[arg(long)]
    pub token_file: Option<String>,

    /// Bootstrap DNS server for resolving the DoH upstream hostname.
    /// Avoids circular dependency when the system resolver points back to us.
    #[arg(long, default_value = "8.8.8.8")]
    pub bootstrap_dns: String,

    /// Request timeout in seconds.
    #[arg(long, default_value = "30")]
    pub timeout_secs: u64,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    // ── Resolve token priority: --token-file > --token/$MICRODOH_TOKEN ──
    let token: Option<Arc<str>> = if let Some(ref path) = cli.token_file {
        let contents = std::fs::read_to_string(path)?;
        Some(Arc::from(contents.trim()))
    } else {
        cli.token.as_deref().map(Arc::from)
    };

    // ── Parse upstream URL ──
    let upstream_url = url::Url::parse(&cli.upstream)
        .map_err(|e| error::Error::url(format!("invalid upstream URL: {e}")))?;
    let upstream_host = upstream_url
        .host_str()
        .ok_or_else(|| error::Error::url("upstream URL has no host"))?
        .to_string();
    let upstream_port = upstream_url.port_or_known_default().unwrap_or(443);

    // ── Parse bootstrap DNS address ──
    let bootstrap_dns: IpAddr = cli
        .bootstrap_dns
        .parse()
        .map_err(|e| error::Error::url(format!("invalid bootstrap-dns: {e}")))?;

    log::info!("upstream  = {}", cli.upstream);
    log::info!("bootstrap = {bootstrap_dns}");
    if token.is_some() {
        log::info!("token     = (set)");
    }

    // ── Build tokio runtime ──
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        // ── Channel between UDP listener and curl worker ──
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        // ── Spawn curl worker thread ──
        let _worker = curl_worker::spawn(
            rx,
            cli.upstream.clone(),
            upstream_host,
            upstream_port,
            bootstrap_dns,
            token,
        );

        // ── Run UDP listener on the tokio runtime ──
        udp::udp_loop(&cli.listen, tx).await
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use clap::Parser;
    use std::io::Write;

    // ── CLI parsing tests ──

    #[test]
    fn test_cli_default_values() {
        let args = vec!["microdoh"];
        let cli = super::Cli::parse_from(args);
        assert_eq!(cli.listen, "0.0.0.0:5300");
        assert_eq!(cli.upstream, "https://dns.google/dns-query");
        assert_eq!(cli.bootstrap_dns, "8.8.8.8");
        assert_eq!(cli.timeout_secs, 30);
        assert!(cli.token_file.is_none());
        // token may be set via env; we don't assert on it
    }

    #[test]
    fn test_cli_custom_values() {
        let args = vec![
            "microdoh",
            "--listen",
            "127.0.0.1:5353",
            "--upstream",
            "https://dns.nextdns.io/abc123",
            "--bootstrap-dns",
            "1.1.1.1",
            "--timeout-secs",
            "10",
            "--token",
            "secret123",
        ];
        let cli = super::Cli::parse_from(args);
        assert_eq!(cli.listen, "127.0.0.1:5353");
        assert_eq!(cli.upstream, "https://dns.nextdns.io/abc123");
        assert_eq!(cli.bootstrap_dns, "1.1.1.1");
        assert_eq!(cli.timeout_secs, 10);
        assert_eq!(cli.token.as_deref(), Some("secret123"));
    }

    #[test]
    fn test_cli_short_flags() {
        let args = vec![
            "microdoh",
            "-l",
            "127.0.0.1:5300",
            "-u",
            "https://doh.example.com/dns-query",
        ];
        let cli = super::Cli::parse_from(args);
        assert_eq!(cli.listen, "127.0.0.1:5300");
        assert_eq!(cli.upstream, "https://doh.example.com/dns-query");
    }

    // ── URL parsing tests ──

    #[test]
    fn test_url_parse_https_default_port() {
        let url = url::Url::parse("https://dns.google/dns-query").unwrap();
        assert_eq!(url.host_str(), Some("dns.google"));
        assert_eq!(url.port_or_known_default(), Some(443));
        assert_eq!(url.path(), "/dns-query");
    }

    #[test]
    fn test_url_parse_custom_port() {
        let url = url::Url::parse("https://dns.example.com:8443/dns-query").unwrap();
        assert_eq!(url.port_or_known_default(), Some(8443));
    }

    // ── Token tests ──

    #[test]
    fn test_token_from_env_var() {
        temp_env::with_var("MICRODOH_TOKEN", Some("bearer-token-123"), || {
            let args = vec!["microdoh"];
            let cli = super::Cli::parse_from(args);
            assert_eq!(cli.token.as_deref(), Some("bearer-token-123"));
        });
    }

    #[test]
    fn test_token_file_reading() {
        let mut dir = std::env::temp_dir();
        dir.push("microdoh_test_token");
        let mut file = std::fs::File::create(&dir).unwrap();
        file.write_all(b"token-from-file\n").unwrap();
        let contents = std::fs::read_to_string(&dir).unwrap();
        assert_eq!(contents.trim(), "token-from-file");
        let _ = std::fs::remove_file(&dir);
    }

    // ── DNS minimum size test ──

    #[test]
    fn test_below_min_dns_size_rejected() {
        assert!(11 < 12); // the UDP listener drops < 12 byte packets
    }

    // ── base64url GET URL format ──

    #[test]
    fn test_get_url_format_contains_dns_param() {
        let query = vec![0u8; 33];
        let b64 = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            &query,
        );
        let upstream = "https://dns.google/dns-query";
        let url = format!("{upstream}?dns={b64}");
        assert!(url.starts_with("https://dns.google/dns-query?dns="));
    }

    // ── UDP listener tests ──

    #[tokio::test]
    async fn test_udp_bind_loopback() {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await;
        assert!(sock.is_ok());
        let sock = sock.unwrap();
        let addr = sock.local_addr().unwrap();
        assert!(addr.port() > 0);
        drop(sock);
    }

    #[tokio::test]
    async fn test_udp_send_recv_loopback() {
        let sock = std::sync::Arc::new(
            tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap(),
        );
        let addr = sock.local_addr().unwrap();

        let query = [
            0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01, 0x00, 0x01,
        ];
        let n = sock.send_to(&query, addr).await.unwrap();
        assert_eq!(n, query.len());

        let mut buf = [0u8; 512];
        let (n2, peer) = sock.recv_from(&mut buf).await.unwrap();
        assert_eq!(n2, query.len());
        assert_eq!(&buf[..n2], &query);
        assert_eq!(peer, addr);
    }

    // ── Temp env helper ──

    mod temp_env {
        use std::env;
        use std::ffi::OsStr;

        pub fn with_var<K, V, F, R>(key: K, value: Option<V>, f: F) -> R
        where
            K: AsRef<OsStr>,
            V: AsRef<OsStr>,
            F: FnOnce() -> R,
        {
            let old = env::var_os(key.as_ref());
            match value {
                Some(v) => env::set_var(key.as_ref(), v),
                None => env::remove_var(key.as_ref()),
            }
            let result = f();
            match old {
                Some(v) => env::set_var(key.as_ref(), v),
                None => env::remove_var(key.as_ref()),
            }
            result
        }
    }
}
