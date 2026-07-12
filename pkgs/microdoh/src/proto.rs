//! DNS wire‑format helpers.
//!
//! Validate incoming DNS queries and apply EDNS0 padding per RFC 8467.

/// Minimum DNS message size (RFC 1035 §4.2.1: header is 12 bytes).
pub const MIN_DNS_LEN: usize = 12;

/// DNS EDNS0 option code for padding (RFC 7830).
const EDNS0_OPT_PAD: u16 = 12;

/// EDNS0 OPT pseudo‑record starts at offset 0 after the header.
/// Structure: root(1) + type(2) + class(2) + TTL(4) + rdlength(2) + options
const EDNS0_HEADER_LEN: usize = 11; // 1 + 2 + 2 + 4 + 2

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("DNS message too short: {0} bytes (minimum 12)")]
    TooShort(usize),

    #[error("QR bit set — not a query")]
    NotAQuery,

    #[error("QDCOUNT={0} — expected at least 1 question")]
    NoQuestion(u16),

    #[error("QDCOUNT={0} — too many questions (max 1 for DoH)")]
    TooManyQuestions(u16),
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate a DNS query per RFC 1035 §4.1 and RFC 8484.
///
/// Checks:
/// * minimum length (12 bytes for the header)
/// * QR bit is 0 (it's a query, not a response)
/// * QDCOUNT is 1 (exactly one question)
pub fn validate_dns_query(buf: &[u8]) -> Result<(), ProtoError> {
    if buf.len() < MIN_DNS_LEN {
        return Err(ProtoError::TooShort(buf.len()));
    }

    // Flags byte 2: QR is bit 7 of byte 2
    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    if flags & 0x8000 != 0 {
        return Err(ProtoError::NotAQuery);
    }

    // QDCOUNT at bytes 4‑5
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]);
    if qdcount == 0 {
        return Err(ProtoError::NoQuestion(qdcount));
    }
    if qdcount > 1 {
        return Err(ProtoError::TooManyQuestions(qdcount));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// EDNS0 Padding (RFC 8467 + RFC 7830)
// ---------------------------------------------------------------------------

/// Pad a DNS query to the next multiple of `block_size` bytes using
/// EDNS0 padding (RFC 7830 option code 12).
///
/// If the message already contains an EDNS0 OPT record (type=41 at the
/// end of the additional section), this appends a padding option.
/// If there's no EDNS0 record, one is added with the padding.
///
/// Returns `true` if padding was applied, `false` if the message already
/// exceeds `block_size` (no padding needed).
pub fn pad_dns_query(buf: &mut Vec<u8>, block_size: usize) -> bool {
    if block_size == 0 || buf.len() >= block_size {
        return false;
    }

    // Check if the message already has an EDNS0 OPT record at the end.
    // OPT record: root(1) + type=41(2) + udp‑size(2) + extended‑rcode(1) +
    //             version(1) + DO+Z(2) + rdlength(2) + options
    let has_edns0 = buf.len() >= EDNS0_HEADER_LEN + 4
        && buf[buf.len() - EDNS0_HEADER_LEN] == 0x00  // root name
        && u16::from_be_bytes([buf[buf.len() - EDNS0_HEADER_LEN + 1],
                                buf[buf.len() - EDNS0_HEADER_LEN + 2]]) == 41; // OPT type

    // Overhead depends on whether an EDNS0 record already exists.
    // Option header = 4 bytes (code + length).  EDNS0 record header = 11 bytes.
    let overhead = if has_edns0 { 4 } else { 11 + 4 };
    let current = buf.len() + overhead;
    if current >= block_size {
        return false; // adding EDNS0 record itself would exceed block_size
    }
    let pad_len = block_size - current;

    if has_edns0 {
        // Update rdlength to include the new padding option.
        let rdlen_offset = buf.len() - 2;
        let old_rdlen = u16::from_be_bytes([buf[rdlen_offset], buf[rdlen_offset + 1]]);
        let new_rdlen = old_rdlen + 4 + pad_len as u16;
        buf[rdlen_offset] = (new_rdlen >> 8) as u8;
        buf[rdlen_offset + 1] = (new_rdlen & 0xFF) as u8;

        // Append padding option: code=12, length=pad_len, zeros
        buf.extend_from_slice(&EDNS0_OPT_PAD.to_be_bytes());
        buf.extend_from_slice(&(pad_len as u16).to_be_bytes());
        buf.resize(buf.len() + pad_len, 0x00);
    } else {
        // Add a complete EDNS0 OPT record with padding.
        // RFC 6891: OPT is in the additional section, so increment ARCOUNT.
        let arcount = u16::from_be_bytes([buf[10], buf[11]]);
        let new_arcount = arcount + 1;
        buf[10] = (new_arcount >> 8) as u8;
        buf[11] = (new_arcount & 0xFF) as u8;

        buf.push(0x00); // root name
        buf.extend_from_slice(&41u16.to_be_bytes()); // type = OPT
        buf.extend_from_slice(&4096u16.to_be_bytes()); // UDP payload size
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // extended rcode / version / DO+Z
        // rdlength: padding option header(4) + pad_len
        let rdlen = 4 + pad_len as u16;
        buf.extend_from_slice(&rdlen.to_be_bytes());
        // Padding option
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

    // ── Validation tests ──

    #[test]
    fn test_validate_ok() {
        // Minimal valid query: 12-byte header + at least 1 question
        let mut buf = vec![0u8; 12];
        buf[4] = 0; buf[5] = 1; // QDCOUNT = 1
        assert!(validate_dns_query(&buf).is_ok());
    }

    #[test]
    fn test_validate_too_short() {
        assert!(matches!(
            validate_dns_query(&[0u8; 11]),
            Err(ProtoError::TooShort(11))
        ));
    }

    #[test]
    fn test_validate_not_a_query() {
        let mut buf = vec![0u8; 12];
        buf[2] = 0x80; // QR bit set
        buf[4] = 0; buf[5] = 1;
        assert!(matches!(
            validate_dns_query(&buf),
            Err(ProtoError::NotAQuery)
        ));
    }

    #[test]
    fn test_validate_no_question() {
        let mut buf = vec![0u8; 12];
        buf[4] = 0; buf[5] = 0; // QDCOUNT = 0
        assert!(matches!(
            validate_dns_query(&buf),
            Err(ProtoError::NoQuestion(0))
        ));
    }

    #[test]
    fn test_validate_too_many_questions() {
        let mut buf = vec![0u8; 12];
        buf[4] = 0; buf[5] = 2; // QDCOUNT = 2
        assert!(matches!(
            validate_dns_query(&buf),
            Err(ProtoError::TooManyQuestions(2))
        ));
    }

    // ── Padding tests ──

    #[test]
    fn test_pad_already_at_boundary() {
        let mut buf = vec![0u8; 128];
        assert!(!pad_dns_query(&mut buf, 128));
        assert_eq!(buf.len(), 128);
    }

    #[test]
    fn test_pad_small_message_no_edns0() {
        let mut buf = vec![0u8; 100];
        assert!(pad_dns_query(&mut buf, 128));
        assert_eq!(buf.len(), 128);
    }

    #[test]
    fn test_pad_preserves_original_data() {
        let mut buf: Vec<u8> = (0..50).collect();
        let original = buf.clone();
        let original_arcount = u16::from_be_bytes([buf[10], buf[11]]);
        pad_dns_query(&mut buf, 128);
        // Bytes 0-9 (ID, flags, QDCOUNT, ANCOUNT, NSCOUNT) are preserved.
        assert_eq!(&buf[..10], &original[..10]);
        // Bytes 10-11 (ARCOUNT) are incremented by 1 (OPT record added).
        assert_eq!(u16::from_be_bytes([buf[10], buf[11]]), original_arcount + 1);
        // Bytes 12..50 (the rest of the original message) are preserved.
        assert_eq!(&buf[12..50], &original[12..50]);
    }

    #[test]
    fn test_pad_zero_block_size() {
        let mut buf = vec![0u8; 10];
        assert!(!pad_dns_query(&mut buf, 0));
    }

    #[test]
    fn test_pad_message_exceeds_block() {
        let mut buf = vec![0u8; 200];
        assert!(!pad_dns_query(&mut buf, 128));
        assert_eq!(buf.len(), 200);
    }

    #[test]
    fn test_pad_length_multiple() {
        // Pad to 128-byte boundary.  Message (30 bytes) + overhead (15) = 45 < 128.
        let mut buf = vec![0u8; 30];
        pad_dns_query(&mut buf, 128);
        assert_eq!(buf.len() % 128, 0);
    }

    #[test]
    fn test_pad_with_existing_edns0() {
        // Build a message with an EDNS0 OPT record
        let mut buf = vec![0u8; 12]; // header
        buf[4] = 0; buf[5] = 1; // QDCOUNT = 1
        buf[10] = 0; buf[11] = 1; // ARCOUNT = 1 (existing OPT record)
        // Add some additional data to simulate existing OPT record
        // OPT record: root(1) + type(2) + class(2) + ttl(4) + rdlen(2) = 11 bytes
        buf.push(0x00); // root name
        buf.extend_from_slice(&41u16.to_be_bytes()); // type = OPT
        buf.extend_from_slice(&4096u16.to_be_bytes()); // UDP payload size
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // extended rcode/version/DO+Z
        buf.extend_from_slice(&0u16.to_be_bytes()); // rdlength = 0 (no options yet)

        let before_len = buf.len();
        assert!(pad_dns_query(&mut buf, 128));
        assert_eq!(buf.len() % 128, 0);
        assert!(buf.len() > before_len);
    }

    // ── ProtoError display ──

    #[test]
    fn test_proto_error_display() {
        assert!(ProtoError::TooShort(5).to_string().contains("too short"));
        assert!(ProtoError::NotAQuery.to_string().contains("not a query"));
        assert!(ProtoError::NoQuestion(0).to_string().contains("QDCOUNT"));
        assert!(ProtoError::TooManyQuestions(3).to_string().contains("QDCOUNT"));
    }
}
