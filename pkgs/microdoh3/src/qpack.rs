//! Minimal QPACK (RFC 9204) encoder/decoder for a DoH/3 client.
//!
//! We advertise `SETTINGS_QPACK_MAX_TABLE_CAPACITY = 0`, so the peer can only
//! use the static table and literal field lines — the dynamic-table forms are
//! rejected on sight. The encoder emits literal-with-literal-name lines only
//! (always valid, a few bytes larger than indexed lines).

use crate::huffman_table::HUFFMAN;
use crate::qpack_static::STATIC_TABLE;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum QpackError {
    #[error("truncated input")]
    Truncated,
    #[error("dynamic table reference (capacity 0 advertised)")]
    DynamicTable,
    #[error("static table index {0} out of range")]
    BadIndex(u64),
    #[error("huffman: {0}")]
    Huffman(#[from] HuffmanError),
}

#[derive(Debug, thiserror::Error)]
pub enum HuffmanError {
    #[error("invalid padding")]
    BadPadding,
    #[error("EOS symbol inside string")]
    EosInString,
    #[error("no matching code / string too long")]
    NoMatch,
}

// ---------------------------------------------------------------------------
// Prefix integers (RFC 7541 §5.1)
// ---------------------------------------------------------------------------

fn write_prefix_int(out: &mut Vec<u8>, prefix_bits: u32, first_byte: u8, value: u64) {
    let max_inline = (1u64 << prefix_bits) - 1;
    if value < max_inline {
        out.push(first_byte | value as u8);
        return;
    }
    out.push(first_byte | max_inline as u8);
    let mut v = value - max_inline;
    while v >= 128 {
        out.push((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn read_prefix_int(buf: &[u8], pos: &mut usize, prefix_bits: u32) -> Result<u64, QpackError> {
    let first = *buf.get(*pos).ok_or(QpackError::Truncated)?;
    *pos += 1;
    let max_inline = (1u64 << prefix_bits) - 1;
    let mut value = (first as u64) & max_inline;
    if value < max_inline {
        return Ok(value);
    }
    let mut shift = 0u32;
    loop {
        let b = *buf.get(*pos).ok_or(QpackError::Truncated)?;
        *pos += 1;
        value = value
            .checked_add(((b & 0x7F) as u64) << shift)
            .ok_or(QpackError::Truncated)?;
        shift += 7;
        if b & 0x80 == 0 {
            return Ok(value);
        }
        if shift > 56 {
            return Err(QpackError::Truncated);
        }
    }
}

// ---------------------------------------------------------------------------
// Encoder (literal-with-literal-name only)
// ---------------------------------------------------------------------------

/// Encode a complete QPACK field section (empty dynamic table).
pub fn encode_field_section(fields: &[(&str, &str)], out: &mut Vec<u8>) {
    // Field section prefix: Required Insert Count = 0, Delta Base = 0.
    out.push(0x00);
    out.push(0x00);
    for (name, value) in fields {
        // Literal Field Line With Literal Name: 001 N=0 H=0 NameLen(3+)
        write_prefix_int(out, 3, 0x20, name.len() as u64);
        out.extend_from_slice(name.as_bytes());
        // Value: H=0, ValueLen(7+)
        write_prefix_int(out, 7, 0x00, value.len() as u64);
        out.extend_from_slice(value.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// A decoded field name: static-table string or literal bytes (ASCII).
#[derive(Debug)]
pub enum NameRef {
    Static(&'static str),
    Wire(Vec<u8>),
}

impl NameRef {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            NameRef::Static(s) => s.as_bytes(),
            NameRef::Wire(v) => v,
        }
    }
}

/// A decoded field value: static-table string or raw wire bytes (+ huffman flag).
#[derive(Debug)]
pub enum ValueRef<'a> {
    Static(&'static str),
    Wire(&'a [u8], bool),
}

impl<'a> ValueRef<'a> {
    /// Decode to bytes (huffman-decoding if flagged).
    pub fn decode(&self) -> Result<Vec<u8>, QpackError> {
        match self {
            ValueRef::Static(s) => Ok(s.as_bytes().to_vec()),
            ValueRef::Wire(b, false) => Ok(b.to_vec()),
            ValueRef::Wire(b, true) => Ok(huffman_decode(b)?),
        }
    }
}

/// One decoded field line.
#[derive(Debug)]
pub struct FieldLine<'a> {
    pub name: NameRef,
    pub value: ValueRef<'a>,
}

/// Iterator over the field lines of an encoded field section.
pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    /// Parse the field-section prefix. Rejects non-empty dynamic tables.
    pub fn new(buf: &'a [u8]) -> Result<Self, QpackError> {
        let mut pos = 0;
        let required_insert_count = read_prefix_int(buf, &mut pos, 8)?;
        let _delta_base = read_prefix_int(buf, &mut pos, 7)?; // sign bit consumed as part of byte
        if required_insert_count != 0 {
            return Err(QpackError::DynamicTable);
        }
        Ok(Self { buf, pos })
    }
}

impl<'a> Iterator for Decoder<'a> {
    type Item = Result<FieldLine<'a>, QpackError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Some((|| {
            if b & 0x80 != 0 {
                // Indexed Field Line: 1 T Index(6+)
                let static_tbl = b & 0x40 != 0;
                if !static_tbl {
                    return Err(QpackError::DynamicTable);
                }
                self.pos -= 1;
                let idx = read_prefix_int(self.buf, &mut self.pos, 6)?;
                let (name, value) = STATIC_TABLE
                    .get(idx as usize)
                    .ok_or(QpackError::BadIndex(idx))?;
                Ok(FieldLine {
                    name: NameRef::Static(name),
                    value: ValueRef::Static(value),
                })
            } else if b & 0xC0 == 0x40 {
                // Literal Field Line With Name Reference: 01 N T Index(4+)
                let static_tbl = b & 0x10 != 0;
                if !static_tbl {
                    return Err(QpackError::DynamicTable);
                }
                self.pos -= 1;
                let idx = read_prefix_int(self.buf, &mut self.pos, 4)?;
                let (name, _) = STATIC_TABLE
                    .get(idx as usize)
                    .ok_or(QpackError::BadIndex(idx))?;
                // Value string: H bit = MSB, len prefix 7.
                let first = *self.buf.get(self.pos).ok_or(QpackError::Truncated)?;
                self.pos += 1;
                let huffman = first & 0x80 != 0;
                self.pos -= 1;
                let len = read_prefix_int(self.buf, &mut self.pos, 7)? as usize;
                let end = self.pos.checked_add(len).ok_or(QpackError::Truncated)?;
                if end > self.buf.len() {
                    return Err(QpackError::Truncated);
                }
                let raw = &self.buf[self.pos..end];
                self.pos = end;
                Ok(FieldLine {
                    name: NameRef::Static(name),
                    value: ValueRef::Wire(raw, huffman),
                })
            } else if b & 0xE0 == 0x20 {
                // Literal Field Line With Literal Name: 001 N H NameLen(3+)
                let huffman_name = b & 0x08 != 0;
                self.pos -= 1;
                let name_len = read_prefix_int(self.buf, &mut self.pos, 3)? as usize;
                let end = self.pos.checked_add(name_len).ok_or(QpackError::Truncated)?;
                if end > self.buf.len() {
                    return Err(QpackError::Truncated);
                }
                let raw_name = &self.buf[self.pos..end];
                self.pos = end;
                let name = if huffman_name {
                    huffman_decode(raw_name)?
                } else {
                    raw_name.to_vec()
                };
                // Value string.
                let first = *self.buf.get(self.pos).ok_or(QpackError::Truncated)?;
                self.pos += 1;
                let huffman_value = first & 0x80 != 0;
                self.pos -= 1;
                let len = read_prefix_int(self.buf, &mut self.pos, 7)? as usize;
                let end = self.pos.checked_add(len).ok_or(QpackError::Truncated)?;
                if end > self.buf.len() {
                    return Err(QpackError::Truncated);
                }
                let raw = &self.buf[self.pos..end];
                self.pos = end;
                Ok(FieldLine {
                    name: NameRef::Wire(name),
                    value: ValueRef::Wire(raw, huffman_value),
                })
            } else {
                // 000xxxxx: post-base / dynamic-table forms — must not appear.
                Err(QpackError::DynamicTable)
            }
        })())
    }
}

// ---------------------------------------------------------------------------
// Huffman decoding (RFC 7541 Appendix B)
// ---------------------------------------------------------------------------

/// Decode a Huffman-coded string. Validates EOS-prefix padding.
pub fn huffman_decode(data: &[u8]) -> Result<Vec<u8>, HuffmanError> {
    let mut out = Vec::with_capacity(data.len());
    let mut acc: u64 = 0;
    let mut nbits: u32 = 0;

    for &byte in data {
        acc = (acc << 8) | byte as u64;
        nbits += 8;
        if nbits > 56 {
            return Err(HuffmanError::NoMatch);
        }
        loop {
            if nbits < 5 {
                break; // shortest code is 5 bits
            }
            let mut matched = false;
            let mut len = 5u32;
            while len <= 30 && len <= nbits {
                let code = (acc >> (nbits - len)) & ((1u64 << len) - 1);
                if let Some(sym) = HUFFMAN
                    .iter()
                    .position(|&(c, l)| l as u32 == len && c as u64 == code)
                {
                    if sym == 256 {
                        return Err(HuffmanError::EosInString);
                    }
                    out.push(sym as u8);
                    nbits -= len;
                    acc &= (1u64 << nbits) - 1;
                    matched = true;
                    break;
                }
                len += 1;
            }
            if !matched {
                break;
            }
        }
    }

    // Padding: at most 7 bits, all ones (EOS prefix).
    if nbits >= 8 {
        return Err(HuffmanError::NoMatch);
    }
    if nbits > 0 && acc != (1u64 << nbits) - 1 {
        return Err(HuffmanError::BadPadding);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_int_roundtrip() {
        for (bits, value) in [(3u32, 0u64), (3, 6), (3, 7), (3, 42), (7, 126), (7, 127), (7, 300), (5, 1337)] {
            let mut buf = Vec::new();
            write_prefix_int(&mut buf, bits, 0, value);
            let mut pos = 0;
            assert_eq!(read_prefix_int(&buf, &mut pos, bits).unwrap(), value, "bits={bits} value={value}");
        }
    }

    #[test]
    fn huffman_rfc7541_vectors() {
        // C.4.1: "www.example.com"
        let enc = [
            0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90, 0xf4, 0xff,
        ];
        assert_eq!(huffman_decode(&enc).unwrap(), b"www.example.com");
        // C.4.2: "no-cache"
        let enc = [0xa8, 0xeb, 0x10, 0x64, 0x9c, 0xbf];
        assert_eq!(huffman_decode(&enc).unwrap(), b"no-cache");
        // C.4.3: "custom-key" / "custom-value"
        let enc = [0x25, 0xa8, 0x49, 0xe9, 0x5b, 0xa9, 0x7d, 0x7f];
        assert_eq!(huffman_decode(&enc).unwrap(), b"custom-key");
        let enc = [0x25, 0xa8, 0x49, 0xe9, 0x5b, 0xb8, 0xe8, 0xb4, 0xbf];
        assert_eq!(huffman_decode(&enc).unwrap(), b"custom-value");
    }

    #[test]
    fn huffman_bad_padding_rejected() {
        assert!(matches!(
            huffman_decode(&[0x00]),
            Err(HuffmanError::BadPadding) | Err(HuffmanError::NoMatch)
        ));
    }

    #[test]
    fn decode_indexed_static_status() {
        // Field section: prefix 00 00, then indexed static 25 (:status 200).
        let section = [0x00, 0x00, 0xD9]; // 0x80|0x40|25
        let mut dec = Decoder::new(&section).unwrap();
        let line = dec.next().unwrap().unwrap();
        assert_eq!(line.name.as_bytes(), b":status");
        assert_eq!(line.value.decode().unwrap(), b"200");
        assert!(dec.next().is_none());
    }

    #[test]
    fn decode_literal_with_name_ref() {
        // :authority (static 0) = "example.com"
        let mut section = vec![0x00, 0x00, 0x50]; // 0x40|0x10|0
        section.push(11);
        section.extend_from_slice(b"example.com");
        let mut dec = Decoder::new(&section).unwrap();
        let line = dec.next().unwrap().unwrap();
        assert_eq!(line.name.as_bytes(), b":authority");
        assert_eq!(line.value.decode().unwrap(), b"example.com");
    }

    #[test]
    fn encode_then_decode_roundtrip() {
        let fields = [
            (":method", "GET"),
            (":scheme", "https"),
            (":authority", "dns.google"),
            (":path", "/dns-query?dns=AAAB"),
            ("accept", "application/dns-message"),
        ];
        let mut buf = Vec::new();
        encode_field_section(&fields, &mut buf);
        let dec = Decoder::new(&buf).unwrap();
        let decoded: Vec<(Vec<u8>, Vec<u8>)> = dec
            .map(|r| {
                let l = r.unwrap();
                (l.name.as_bytes().to_vec(), l.value.decode().unwrap())
            })
            .collect();
        let want: Vec<(Vec<u8>, Vec<u8>)> = fields
            .iter()
            .map(|(n, v)| (n.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect();
        assert_eq!(decoded, want);
    }

    #[test]
    fn dynamic_table_rejected() {
        // RIC != 0
        assert!(matches!(
            Decoder::new(&[0x01, 0x00]),
            Err(QpackError::DynamicTable)
        ));
        // Indexed dynamic line (T=0)
        // Indexed dynamic line (T=0): 0x80 = 1 0 000000
        let section = [0x00, 0x00, 0x80];
        let mut dec2 = Decoder::new(&section).unwrap();
        assert!(matches!(
            dec2.next().unwrap(),
            Err(QpackError::DynamicTable)
        ));
    }

    #[test]
    fn static_table_spot_check() {
        assert_eq!(STATIC_TABLE[0], (":authority", ""));
        assert_eq!(STATIC_TABLE[1], (":path", "/"));
        assert_eq!(STATIC_TABLE[17], (":method", "GET"));
        assert_eq!(STATIC_TABLE[23], (":scheme", "https"));
        assert_eq!(STATIC_TABLE[25], (":status", "200"));
        assert_eq!(STATIC_TABLE[30], ("accept", "application/dns-message"));
        assert_eq!(STATIC_TABLE.len(), 99);
    }
}
