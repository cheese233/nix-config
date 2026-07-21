//! Encode-only base64url without padding (RFC 4648 §5), as required for the
//! RFC 8484 `dns` query parameter.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Exact encoded length for `n` input bytes (no padding).
pub const fn encoded_len(n: usize) -> usize {
    (n / 3) * 4
        + match n % 3 {
            0 => 0,
            1 => 2,
            _ => 3,
        }
}

/// Encode `input` into `out`, returning the number of bytes written.
/// `out` must be at least `encoded_len(input.len())` bytes.
pub fn encode_into(input: &[u8], out: &mut [u8]) -> usize {
    let mut o = 0;
    let mut chunks = input.chunks_exact(3);
    for c in &mut chunks {
        let n = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | c[2] as u32;
        out[o] = ALPHABET[(n >> 18) as usize & 63];
        out[o + 1] = ALPHABET[(n >> 12) as usize & 63];
        out[o + 2] = ALPHABET[(n >> 6) as usize & 63];
        out[o + 3] = ALPHABET[n as usize & 63];
        o += 4;
    }
    match chunks.remainder() {
        [a] => {
            let n = (*a as u32) << 16;
            out[o] = ALPHABET[(n >> 18) as usize & 63];
            out[o + 1] = ALPHABET[(n >> 12) as usize & 63];
            o += 2;
        }
        [a, b] => {
            let n = ((*a as u32) << 16) | ((*b as u32) << 8);
            out[o] = ALPHABET[(n >> 18) as usize & 63];
            out[o + 1] = ALPHABET[(n >> 12) as usize & 63];
            out[o + 2] = ALPHABET[(n >> 6) as usize & 63];
            o += 3;
        }
        _ => {}
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(data: &[u8]) -> String {
        let mut buf = vec![0u8; encoded_len(data.len())];
        let n = encode_into(data, &mut buf);
        assert_eq!(n, buf.len());
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn rfc8484_vector() {
        // RFC 8484 §4.1.1: A record query for www.example.com
        let query: Vec<u8> = (0..66)
            .step_by(2)
            .map(|i| u8::from_str_radix(&"00000100000100000000000003777777076578616d706c6503636f6d0000010001"[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(encode(&query), "AAABAAABAAAAAAAAA3d3dwdleGFtcGxlA2NvbQAAAQAB");
    }

    #[test]
    fn rfc4648_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg");
        assert_eq!(encode(b"fo"), "Zm8");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg");
        assert_eq!(encode(b"fooba"), "Zm9vYmE");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn urlsafe_alphabet() {
        // 0x3f, 0x7f → uses '-' and '_', never '+'/'/'
        let out = encode(&[0x3f, 0x7f]);
        assert!(!out.contains('+'));
        assert!(!out.contains('/'));
        assert!(!out.contains('='));
    }

    #[test]
    fn encoded_len_check() {
        assert_eq!(encoded_len(0), 0);
        assert_eq!(encoded_len(1), 2);
        assert_eq!(encoded_len(2), 3);
        assert_eq!(encoded_len(3), 4);
        assert_eq!(encoded_len(4), 6);
    }
}
