//! DNS wire-format helpers (byte-level; no full parsing in the hot path).
//!
//! - validate incoming queries (minimum length, QR=0, QDCOUNT=1)
//! - EDNS0 padding (RFC 8467 + RFC 7830)
//! - SERVFAIL synthesis for fast-fail responses

/// Minimum DNS message size (RFC 1035 §4.2.1: header is 12 bytes).
pub const MIN_DNS_LEN: usize = 12;

/// DNS EDNS0 option code for padding (RFC 7830).
const EDNS0_OPT_PAD: u16 = 12;

/// EDNS0 OPT pseudo-record header length: root(1) + type(2) + class(2) + TTL(4) + rdlength(2).
const EDNS0_HEADER_LEN: usize = 11;

#[derive(Debug, thiserror::Error)]
pub enum DnsError {
    #[error("DNS message too short: {0} bytes (minimum 12)")]
    TooShort(usize),

    #[error("QR bit set — not a query")]
    NotAQuery,

    #[error("QDCOUNT={0} — expected at least 1 question")]
    NoQuestion(u16),

    #[error("QDCOUNT={0} — too many questions (max 1 for DoH)")]
    TooManyQuestions(u16),

    #[error("malformed question section")]
    MalformedQuestion,
}

// ---------------------------------------------------------------------------
// Validation (header-only, zero-alloc)
// ---------------------------------------------------------------------------

/// Validate a DNS query per RFC 1035 §4.1 and RFC 8484: minimum length,
/// QR bit clear, exactly one question.
pub fn validate_query(buf: &[u8]) -> Result<(), DnsError> {
    if buf.len() < MIN_DNS_LEN {
        return Err(DnsError::TooShort(buf.len()));
    }
    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    if flags & 0x8000 != 0 {
        return Err(DnsError::NotAQuery);
    }
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]);
    if qdcount == 0 {
        return Err(DnsError::NoQuestion(qdcount));
    }
    if qdcount > 1 {
        return Err(DnsError::TooManyQuestions(qdcount));
    }
    Ok(())
}

/// Find the end offset of the question section (header + QNAME + QTYPE + QCLASS).
/// Handles label sequences; rejects pointers/truncated names (queries from
/// local resolvers are never compressed, but be safe).
fn question_end(buf: &[u8]) -> Result<usize, DnsError> {
    let mut pos = MIN_DNS_LEN;
    loop {
        let len = *buf.get(pos).ok_or(DnsError::MalformedQuestion)?;
        if len == 0 {
            return Ok(pos + 1 + 4); // root label + QTYPE + QCLASS
        }
        if len & 0xC0 != 0 {
            // Compression pointer or invalid label — not valid in a fresh query.
            return Err(DnsError::MalformedQuestion);
        }
        pos += 1 + len as usize;
        if pos > buf.len() {
            return Err(DnsError::MalformedQuestion);
        }
    }
}

// ---------------------------------------------------------------------------
// SERVFAIL synthesis (fast-fail)
// ---------------------------------------------------------------------------

/// Build a SERVFAIL response for `query` into `out`.
/// Copies header + question only; sets QR=1, RCODE=2, zeroes all counts
/// except QDCOUNT. Returns false if the query is malformed.
pub fn build_servfail(query: &[u8], out: &mut Vec<u8>) -> bool {
    let end = match question_end(query) {
        Ok(e) if e <= query.len() => e,
        _ => {
            // Malformed question: respond with just the header.
            if query.len() < MIN_DNS_LEN {
                return false;
            }
            MIN_DNS_LEN
        }
    };
    out.clear();
    out.extend_from_slice(&query[..end]);
    // QR=1 (response), keep opcode/RD, RA=1, RCODE=2 (SERVFAIL)
    out[2] |= 0x80;
    out[3] = (out[3] & 0x70) | 0x80 | 0x02;
    // ANCOUNT = NSCOUNT = ARCOUNT = 0
    out[6..12].fill(0);
    true
}

// ---------------------------------------------------------------------------
// EDNS0 Padding (RFC 8467 + RFC 7830)
// ---------------------------------------------------------------------------

/// Pad a DNS query to the next multiple of `block_size` bytes using EDNS0
/// padding. Returns `true` if padding was applied.
pub fn pad_query(buf: &mut Vec<u8>, block_size: usize) -> bool {
    if block_size == 0 || buf.len() >= block_size {
        return false;
    }

    let has_edns0 = buf.len() >= EDNS0_HEADER_LEN + 4
        && buf[buf.len() - EDNS0_HEADER_LEN] == 0x00 // root name
        && u16::from_be_bytes([
            buf[buf.len() - EDNS0_HEADER_LEN + 1],
            buf[buf.len() - EDNS0_HEADER_LEN + 2],
        ]) == 41; // OPT type

    let overhead = if has_edns0 { 4 } else { 11 + 4 };
    let current = buf.len() + overhead;
    if current >= block_size {
        return false;
    }
    let pad_len = block_size - current;

    if has_edns0 {
        let rdlen_offset = buf.len() - 2;
        let old_rdlen = u16::from_be_bytes([buf[rdlen_offset], buf[rdlen_offset + 1]]);
        let new_rdlen = old_rdlen + 4 + pad_len as u16;
        buf[rdlen_offset] = (new_rdlen >> 8) as u8;
        buf[rdlen_offset + 1] = (new_rdlen & 0xFF) as u8;

        buf.extend_from_slice(&EDNS0_OPT_PAD.to_be_bytes());
        buf.extend_from_slice(&(pad_len as u16).to_be_bytes());
        buf.resize(buf.len() + pad_len, 0x00);
    } else {
        let arcount = u16::from_be_bytes([buf[10], buf[11]]);
        let new_arcount = arcount + 1;
        buf[10] = (new_arcount >> 8) as u8;
        buf[11] = (new_arcount & 0xFF) as u8;

        buf.push(0x00); // root name
        buf.extend_from_slice(&41u16.to_be_bytes()); // type = OPT
        buf.extend_from_slice(&4096u16.to_be_bytes()); // UDP payload size
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // extended rcode/version/DO+Z
        let rdlen = 4 + pad_len as u16;
        buf.extend_from_slice(&rdlen.to_be_bytes());
        buf.extend_from_slice(&EDNS0_OPT_PAD.to_be_bytes());
        buf.extend_from_slice(&(pad_len as u16).to_be_bytes());
        buf.resize(buf.len() + pad_len, 0x00);
    }

    true
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid query: 12-byte header + question for "example.com" A IN.
    fn sample_query() -> Vec<u8> {
        let mut q = vec![
            0xAB, 0xCD, // ID
            0x01, 0x00, // RD
            0x00, 0x01, // QDCOUNT=1
            0x00, 0x00, // ANCOUNT
            0x00, 0x00, // NSCOUNT
            0x00, 0x00, // ARCOUNT
        ];
        q.push(7);
        q.extend_from_slice(b"example");
        q.push(3);
        q.extend_from_slice(b"com");
        q.push(0);
        q.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // A IN
        q
    }

    #[test]
    fn validate_ok() {
        assert!(validate_query(&sample_query()).is_ok());
    }

    #[test]
    fn validate_too_short() {
        assert!(matches!(
            validate_query(&[0u8; 11]),
            Err(DnsError::TooShort(11))
        ));
    }

    #[test]
    fn validate_not_a_query() {
        let mut q = sample_query();
        q[2] |= 0x80;
        assert!(matches!(validate_query(&q), Err(DnsError::NotAQuery)));
    }

    #[test]
    fn validate_no_question() {
        let mut q = sample_query();
        q[4] = 0;
        q[5] = 0;
        assert!(matches!(validate_query(&q), Err(DnsError::NoQuestion(0))));
    }

    #[test]
    fn validate_two_questions() {
        let mut q = sample_query();
        q[5] = 2;
        assert!(matches!(
            validate_query(&q),
            Err(DnsError::TooManyQuestions(2))
        ));
    }

    #[test]
    fn question_end_correct() {
        let q = sample_query();
        assert_eq!(question_end(&q).unwrap(), q.len());
    }

    #[test]
    fn question_end_rejects_pointer() {
        let mut q = sample_query();
        q[12] = 0xC0; // compression pointer in QNAME
        assert!(matches!(
            question_end(&q),
            Err(DnsError::MalformedQuestion)
        ));
    }

    #[test]
    fn servfail_structure() {
        let q = sample_query();
        let mut out = Vec::new();
        assert!(build_servfail(&q, &mut out));
        assert_eq!(out.len(), q.len()); // header + question, no extra sections
        // ID preserved
        assert_eq!(&out[0..2], &[0xAB, 0xCD]);
        // QR=1
        assert!(out[2] & 0x80 != 0);
        // RCODE=2
        assert_eq!(out[3] & 0x0F, 2);
        // QDCOUNT=1, others 0
        assert_eq!(&out[4..6], &[0, 1]);
        assert_eq!(&out[6..12], &[0; 6]);
        // Question preserved verbatim
        assert_eq!(&out[12..], &q[12..]);
    }

    #[test]
    fn servfail_malformed() {
        let mut out = Vec::new();
        assert!(!build_servfail(&[0u8; 5], &mut out));
    }

    #[test]
    fn servfail_rd_preserved() {
        let q = sample_query();
        let mut out = Vec::new();
        build_servfail(&q, &mut out);
        // RD bit (0x01 in byte 2) preserved from query
        assert!(out[2] & 0x01 != 0);
        // RA set
        assert!(out[3] & 0x80 != 0);
    }

    // ── Padding tests (ported from microdoh proto.rs) ──

    #[test]
    fn pad_already_at_boundary() {
        let mut buf = vec![0u8; 128];
        assert!(!pad_query(&mut buf, 128));
        assert_eq!(buf.len(), 128);
    }

    #[test]
    fn pad_small_message() {
        let mut buf = vec![0u8; 100];
        assert!(pad_query(&mut buf, 128));
        assert_eq!(buf.len(), 128);
    }

    #[test]
    fn pad_preserves_original_data() {
        let mut buf: Vec<u8> = (0..50).collect();
        let original = buf.clone();
        let original_arcount = u16::from_be_bytes([buf[10], buf[11]]);
        pad_query(&mut buf, 128);
        assert_eq!(&buf[..10], &original[..10]);
        assert_eq!(
            u16::from_be_bytes([buf[10], buf[11]]),
            original_arcount + 1
        );
        assert_eq!(&buf[12..50], &original[12..50]);
    }

    #[test]
    fn pad_message_exceeds_block() {
        let mut buf = vec![0u8; 200];
        assert!(!pad_query(&mut buf, 128));
        assert_eq!(buf.len(), 200);
    }

    #[test]
    fn pad_length_multiple() {
        let mut buf = vec![0u8; 30];
        pad_query(&mut buf, 128);
        assert_eq!(buf.len() % 128, 0);
    }

    #[test]
    fn pad_with_existing_edns0() {
        let mut buf = vec![0u8; 12];
        buf[4] = 0;
        buf[5] = 1;
        buf[10] = 0;
        buf[11] = 1;
        buf.push(0x00);
        buf.extend_from_slice(&41u16.to_be_bytes());
        buf.extend_from_slice(&4096u16.to_be_bytes());
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        buf.extend_from_slice(&0u16.to_be_bytes());

        let before_len = buf.len();
        assert!(pad_query(&mut buf, 128));
        assert_eq!(buf.len() % 128, 0);
        assert!(buf.len() > before_len);
    }
}
