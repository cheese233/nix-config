use std::fmt;

/// Unified error type for microdoh.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("curl: {0}")]
    Curl(#[from] curl::Error),

    #[error("curl multi: {0}")]
    CurlMulti(String),

    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),

    #[error("DNS bootstrap: {0}")]
    DnsBootstrap(String),

    #[error("invalid URL: {0}")]
    Url(String),

    #[error("internal: {0}")]
    #[allow(dead_code)]
    Internal(String),
}

impl Error {
    pub fn curl_multi(msg: impl fmt::Display) -> Self {
        Error::CurlMulti(msg.to_string())
    }

    pub fn dns_bootstrap(msg: impl fmt::Display) -> Self {
        Error::DnsBootstrap(msg.to_string())
    }

    pub fn url(msg: impl fmt::Display) -> Self {
        Error::Url(msg.to_string())
    }

    #[allow(dead_code)]
    pub fn internal(msg: impl fmt::Display) -> Self {
        Error::Internal(msg.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display_curl() {
        let curl_err = curl::Error::new(curl_sys::CURLE_URL_MALFORMAT);
        let err: Error = curl_err.into();
        assert!(format!("{err}").contains("curl"));
    }

    #[test]
    fn test_error_display_curl_multi() {
        let err = Error::curl_multi("multi oops");
        assert!(format!("{err}").contains("multi oops"));
    }

    #[test]
    fn test_error_display_dns_bootstrap() {
        let err = Error::dns_bootstrap("host not found");
        assert!(format!("{err}").contains("host not found"));
    }

    #[test]
    fn test_error_display_url() {
        let err = Error::url("bad URL");
        assert!(format!("{err}").contains("bad URL"));
    }

    #[test]
    fn test_error_display_internal() {
        let err = Error::internal("invariant violated");
        assert!(format!("{err}").contains("invariant violated"));
    }

    #[test]
    fn test_error_from_io() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err: Error = io.into();
        assert!(format!("{err}").contains("file missing"));
    }

    #[test]
    fn test_error_from_curl_codes() {
        let codes = [
            curl_sys::CURLE_URL_MALFORMAT,
            curl_sys::CURLE_COULDNT_RESOLVE_HOST,
            curl_sys::CURLE_HTTP_RETURNED_ERROR,
        ];
        for code in codes {
            let curl_err = curl::Error::new(code);
            let err: Error = curl_err.into();
            assert!(format!("{err}").contains("curl"), "code={code}");
        }
    }

    #[test]
    fn test_result_ok() {
        let r: Result<i32> = Ok(42);
        assert_eq!(r.unwrap(), 42);
    }

    #[test]
    fn test_result_err() {
        let r: Result<i32> = Err(Error::internal("test error"));
        assert!(r.is_err());
    }

    #[test]
    fn test_dead_code_internal_variant_accessible() {
        // Verify Internal is constructable via the helper.
        let _e = Error::internal("unit test");
    }
}
