//! Minimal `https://host[:port]/path[?query]` parser for the DoH upstream URL.
//! Replaces the `url` crate (which pulls in idna/unicode machinery we don't need).

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpsUrl {
    /// Hostname without brackets (used for TLS SNI / DNS resolution).
    pub host: String,
    /// Port (default 443).
    pub port: u16,
    /// Path including optional query string (e.g. `/dns-query?x=1`).
    pub path: String,
    /// Value for the `:authority` pseudo-header (`host` or `host:port`;
    /// bracketed for IPv6 literals).
    pub authority: String,
}

#[derive(Debug, thiserror::Error)]
pub enum UrlError {
    #[error("URL must use https:// scheme")]
    NotHttps,
    #[error("URL has no host")]
    NoHost,
    #[error("invalid port")]
    BadPort,
    #[error("userinfo and fragments are not supported")]
    Unsupported,
}

impl HttpsUrl {
    pub fn parse(s: &str) -> Result<Self, UrlError> {
        let rest = s.strip_prefix("https://").ok_or(UrlError::NotHttps)?;
        if rest.contains('@') || rest.contains('#') {
            return Err(UrlError::Unsupported);
        }
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err(UrlError::NoHost);
        }

        let (host, port) = if let Some(a) = authority.strip_prefix('[') {
            // IPv6 literal: [::1] or [::1]:8443
            let end = a.find(']').ok_or(UrlError::NoHost)?;
            let h = &a[..end];
            let tail = &a[end + 1..];
            let p = match tail.strip_prefix(':') {
                Some(ps) => ps.parse::<u16>().map_err(|_| UrlError::BadPort)?,
                None if tail.is_empty() => 443,
                _ => return Err(UrlError::BadPort),
            };
            (h.to_string(), p)
        } else {
            match authority.rsplit_once(':') {
                Some((h, ps)) => {
                    let p = ps.parse::<u16>().map_err(|_| UrlError::BadPort)?;
                    (h.to_string(), p)
                }
                None => (authority.to_string(), 443),
            }
        };
        if host.is_empty() {
            return Err(UrlError::NoHost);
        }

        let authority_out = if host.contains(':') {
            // Re-bracket IPv6 for :authority
            if port == 443 {
                format!("[{host}]")
            } else {
                format!("[{host}]:{port}")
            }
        } else if port == 443 {
            host.clone()
        } else {
            format!("{host}:{port}")
        };

        Ok(Self {
            host,
            port,
            path: path.to_string(),
            authority: authority_out,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let u = HttpsUrl::parse("https://dns.google/dns-query").unwrap();
        assert_eq!(u.host, "dns.google");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/dns-query");
        assert_eq!(u.authority, "dns.google");
    }

    #[test]
    fn custom_port() {
        let u = HttpsUrl::parse("https://dns.example.com:8443/dns-query").unwrap();
        assert_eq!(u.host, "dns.example.com");
        assert_eq!(u.port, 8443);
        assert_eq!(u.authority, "dns.example.com:8443");
    }

    #[test]
    fn no_path() {
        let u = HttpsUrl::parse("https://dns.example.com").unwrap();
        assert_eq!(u.path, "/");
    }

    #[test]
    fn path_with_query() {
        let u = HttpsUrl::parse("https://doh.example.com/dns-query?foo=bar").unwrap();
        assert_eq!(u.path, "/dns-query?foo=bar");
    }

    #[test]
    fn ipv6_literal() {
        let u = HttpsUrl::parse("https://[2001:4860:4860::8888]/dns-query").unwrap();
        assert_eq!(u.host, "2001:4860:4860::8888");
        assert_eq!(u.port, 443);
        assert_eq!(u.authority, "[2001:4860:4860::8888]");
    }

    #[test]
    fn ipv6_literal_port() {
        let u = HttpsUrl::parse("https://[::1]:8443/dns-query").unwrap();
        assert_eq!(u.host, "::1");
        assert_eq!(u.port, 8443);
        assert_eq!(u.authority, "[::1]:8443");
    }

    #[test]
    fn rejects_http() {
        assert!(matches!(
            HttpsUrl::parse("http://dns.example.com/"),
            Err(UrlError::NotHttps)
        ));
    }

    #[test]
    fn rejects_userinfo() {
        assert!(matches!(
            HttpsUrl::parse("https://user@dns.example.com/"),
            Err(UrlError::Unsupported)
        ));
    }

    #[test]
    fn rejects_empty_host() {
        assert!(matches!(
            HttpsUrl::parse("https:///dns-query"),
            Err(UrlError::NoHost)
        ));
    }

    #[test]
    fn rejects_bad_port() {
        assert!(matches!(
            HttpsUrl::parse("https://dns.example.com:abc/"),
            Err(UrlError::BadPort)
        ));
    }
}
