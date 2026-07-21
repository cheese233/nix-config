//! Minimal HTTP/3 (RFC 9114) client layer for DoH (RFC 8484 over H3).
//!
//! - One client control stream with SETTINGS (QPACK dynamic table disabled).
//! - One bidirectional stream per request: HEADERS (+ DATA for POST), then FIN.
//! - Responses are parsed incrementally; we emit the body on stream FIN
//!   (the FIN arrives in the same QUIC packet as the DATA in practice).
//! - GOAWAY, unknown frames (grease), and QPACK encoder/decoder streams are
//!   handled by skipping/draining.

use std::collections::HashMap;

use crate::qpack;

// ---------------------------------------------------------------------------
// QUIC varints (RFC 9000 §16)
// ---------------------------------------------------------------------------

/// Append a QUIC variable-length integer.
pub fn varint_encode(v: u64, out: &mut Vec<u8>) {
    if v < 1 << 6 {
        out.push(v as u8);
    } else if v < 1 << 14 {
        out.extend_from_slice(&(v as u16 | 0x4000).to_be_bytes());
    } else if v < 1 << 30 {
        out.extend_from_slice(&(v as u32 | 0x8000_0000).to_be_bytes());
    } else {
        out.extend_from_slice(&(v | 0xC000_0000_0000_0000).to_be_bytes());
    }
}

/// Decode a QUIC varint, returning (value, bytes consumed), or None if incomplete.
pub fn varint_decode(buf: &[u8]) -> Option<(u64, usize)> {
    let first = *buf.first()?;
    let len = 1usize << (first >> 6);
    if buf.len() < len {
        return None;
    }
    let mut v = (first & 0x3F) as u64;
    for &b in &buf[1..len] {
        v = (v << 8) | b as u64;
    }
    Some((v, len))
}

// ---------------------------------------------------------------------------
// H3 constants
// ---------------------------------------------------------------------------

const FRAME_DATA: u64 = 0x00;
const FRAME_HEADERS: u64 = 0x01;
const FRAME_SETTINGS: u64 = 0x04;
const FRAME_GOAWAY: u64 = 0x07;

const STREAM_TYPE_CONTROL: u64 = 0x00;

const SETTINGS_QPACK_MAX_TABLE_CAPACITY: u64 = 0x01;
const SETTINGS_QPACK_BLOCKED_STREAMS: u64 = 0x07;

// ---------------------------------------------------------------------------
// Outbound encoding
// ---------------------------------------------------------------------------

/// Client connection preamble: control stream type + SETTINGS frame.
/// Must be the first bytes sent on the first client-initiated uni stream.
pub fn client_preamble() -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    varint_encode(STREAM_TYPE_CONTROL, &mut out);
    // SETTINGS payload: QPACK_MAX_TABLE_CAPACITY=0, QPACK_BLOCKED_STREAMS=0
    let mut payload = Vec::with_capacity(4);
    varint_encode(SETTINGS_QPACK_MAX_TABLE_CAPACITY, &mut payload);
    varint_encode(0, &mut payload);
    varint_encode(SETTINGS_QPACK_BLOCKED_STREAMS, &mut payload);
    varint_encode(0, &mut payload);
    varint_encode(FRAME_SETTINGS, &mut out);
    varint_encode(payload.len() as u64, &mut out);
    out.extend_from_slice(&payload);
    out
}

/// Encode an H3 request (HEADERS frame, plus DATA frame for POST with `body`).
/// `path` includes the query string (e.g. `/dns-query?dns=...` for GET).
pub fn encode_request(
    out: &mut Vec<u8>,
    method: &str,
    authority: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
) {
    let mut fields: Vec<(&str, &str)> = vec![
        (":method", method),
        (":scheme", "https"),
        (":authority", authority),
        (":path", path),
        ("accept", "application/dns-message"),
    ];
    let auth;
    if let Some(t) = token {
        auth = format!("Bearer {t}");
        fields.push(("authorization", &auth));
    }
    let mut block = Vec::with_capacity(256);
    qpack::encode_field_section(&fields, &mut block);
    varint_encode(FRAME_HEADERS, out);
    varint_encode(block.len() as u64, out);
    out.extend_from_slice(&block);

    if let Some(b) = body {
        varint_encode(FRAME_DATA, out);
        varint_encode(b.len() as u64, out);
        out.extend_from_slice(b);
    }
}

// ---------------------------------------------------------------------------
// Incremental frame parser
// ---------------------------------------------------------------------------

/// Accumulates stream bytes and yields complete frames.
#[derive(Default)]
pub struct FrameParser {
    buf: Vec<u8>,
}

impl FrameParser {
    pub fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Pop the next complete frame as (type, payload), or None if incomplete.
    pub fn next_frame(&mut self) -> Option<(u64, Vec<u8>)> {
        let (ty, n1) = varint_decode(&self.buf)?;
        let rest = &self.buf[n1..];
        let (len, n2) = varint_decode(rest)?;
        let len = len as usize;
        let start = n1 + n2;
        if self.buf.len() < start + len {
            return None;
        }
        let payload = self.buf[start..start + len].to_vec();
        self.buf.drain(..start + len);
        Some((ty, payload))
    }
}

// ---------------------------------------------------------------------------
// H3 connection state
// ---------------------------------------------------------------------------

/// Per-stream state.
enum StreamState {
    /// Server-initiated uni stream; type varint not yet consumed.
    ServerUni {
        ty: Option<u64>,
        parser: FrameParser,
    },
    /// Drain bytes without parsing (QPACK streams, push, unknown types).
    Sink,
    /// One of our request streams.
    Response {
        parser: FrameParser,
        status: Option<u16>,
        body: Vec<u8>,
        /// Complete DATA frame received.
        got_data: bool,
        /// Failure already reported; suppress further events.
        failed: bool,
    },
}

/// Events emitted by the H3 layer.
#[derive(Debug)]
pub enum H3Event {
    /// Complete DNS response body for a request stream (status 200).
    Response { stream: u64, body: Vec<u8> },
    /// The request failed at the H3 level (bad status, malformed, reset).
    Failed { stream: u64 },
    /// GOAWAY received from the server.
    Goaway,
}

pub struct H3 {
    streams: HashMap<u64, StreamState>,
    goaway: bool,
}

impl H3 {
    pub fn new() -> Self {
        Self {
            streams: HashMap::new(),
            goaway: false,
        }
    }

    #[allow(dead_code)]
    pub fn goaway_received(&self) -> bool {
        self.goaway
    }

    /// Register a freshly opened request stream.
    pub fn register_request(&mut self, stream: u64) {
        self.streams.insert(
            stream,
            StreamState::Response {
                parser: FrameParser::default(),
                status: None,
                body: Vec::with_capacity(512),
                got_data: false,
                failed: false,
            },
        );
    }

    /// Forget a stream whose lifecycle is over (response delivered or failed).
    #[allow(dead_code)]
    pub fn remove(&mut self, stream: u64) {
        self.streams.remove(&stream);
    }

    /// Feed received bytes for a stream. Unregistered uni streams are
    /// auto-classified by their type varint.
    pub fn feed(&mut self, stream: u64, data: &[u8], events: &mut Vec<H3Event>) {
        let state = self.streams.entry(stream).or_insert_with(|| {
            // Only server-initiated uni streams arrive unregistered.
            StreamState::ServerUni {
                ty: None,
                parser: FrameParser::default(),
            }
        });

        // Resolve the type of server uni streams first.
        if let StreamState::ServerUni { ty, parser } = state {
            if ty.is_none() {
                if let Some((t, n)) = varint_decode(data) {
                    *ty = Some(t);
                    // Reclassify: control stream parses frames, everything else is sunk.
                    if t == STREAM_TYPE_CONTROL {
                        parser.feed(&data[n..]);
                        self.process_server_uni(stream, events);
                    } else {
                        // QPACK encoder/decoder streams, push, unknown: drain.
                        self.streams.insert(stream, StreamState::Sink);
                    }
                } else if data.is_empty() {
                    // Nothing yet; wait for more bytes.
                } else {
                    // Can't decode type varint yet — buffer via parser scratch.
                    // varint is at most 8 bytes; feed whole chunk into parser and retry later.
                    parser.feed(data);
                    // Try reading buffered type.
                    if let StreamState::ServerUni { ty, parser } =
                        self.streams.get_mut(&stream).unwrap()
                    {
                        if ty.is_none() {
                            if let Some((t, _n)) = varint_decode(&parser.buf) {
                                *ty = Some(t);
                            }
                        }
                    }
                    // Extract buffered type+frames.
                    if matches!(
                        self.streams.get(&stream),
                        Some(StreamState::ServerUni { ty: Some(_), .. })
                    ) {
                        if let Some(StreamState::ServerUni { ty, parser }) =
                            self.streams.remove(&stream)
                        {
                            let t = ty.unwrap();
                            let (_t, n) = varint_decode(&parser.buf).unwrap();
                            let rest = parser.buf[n..].to_vec();
                            if t == STREAM_TYPE_CONTROL {
                                let mut p = FrameParser::default();
                                p.feed(&rest);
                                self.streams.insert(
                                    stream,
                                    StreamState::ServerUni { ty: Some(t), parser: p },
                                );
                                self.process_server_uni(stream, events);
                            } else {
                                self.streams.insert(stream, StreamState::Sink);
                            }
                        }
                    }
                }
                return;
            }
            parser.feed(data);
            self.process_server_uni(stream, events);
            return;
        }

        match state {
            StreamState::Response {
                parser,
                status,
                body,
                got_data,
                failed,
            } => {
                parser.feed(data);
                let mut st = std::mem::take(status);
                let mut bd = std::mem::take(body);
                let mut gd = *got_data;
                let mut fl = *failed;
                while let Some((fty, payload)) = parser.next_frame() {
                    match fty {
                        FRAME_HEADERS => {
                            match parse_status(&payload) {
                                Ok(code) => st = Some(code),
                                Err(_) => fl = true,
                            }
                        }
                        FRAME_DATA => {
                            bd.extend_from_slice(&payload);
                            gd = true;
                        }
                        _ => {} // skip unknown/trailer frames
                    }
                }
                // Write state back.
                if let Some(StreamState::Response {
                    status,
                    body,
                    got_data,
                    failed,
                    ..
                }) = self.streams.get_mut(&stream)
                {
                    *status = st;
                    *body = bd;
                    *got_data = gd;
                    *failed = fl;
                }
            }
            StreamState::Sink => {}
            StreamState::ServerUni { .. } => unreachable!(),
        }
    }

    fn process_server_uni(&mut self, stream: u64, events: &mut Vec<H3Event>) {
        if let Some(StreamState::ServerUni { ty, parser }) = self.streams.get_mut(&stream) {
            if *ty != Some(STREAM_TYPE_CONTROL) {
                return;
            }
            while let Some((fty, _payload)) = parser.next_frame() {
                match fty {
                    FRAME_GOAWAY => {
                        self.goaway = true;
                        events.push(H3Event::Goaway);
                    }
                    _ => {} // SETTINGS and others: ignore
                }
            }
        }
    }

    /// Stream finished (FIN). Emits the response if a complete DATA frame was
    /// received with status 200, otherwise reports failure.
    pub fn finish(&mut self, stream: u64, events: &mut Vec<H3Event>) {
        match self.streams.remove(&stream) {
            Some(StreamState::Response {
                status,
                body,
                got_data,
                failed,
                parser,
            }) => {
                if failed {
                    events.push(H3Event::Failed { stream });
                } else if got_data && status == Some(200) {
                    events.push(H3Event::Response { stream, body });
                } else {
                    log::debug!("h3 finish stream {stream}: status={status:?} got_data={got_data} body={} parser_buf={}", body.len(), parser.buf.len());
                    events.push(H3Event::Failed { stream });
                }
            }
            _ => {} // server uni / sink: nothing to do
        }
    }

    /// Stream reset by peer or write side stopped.
    pub fn reset(&mut self, stream: u64, events: &mut Vec<H3Event>) {
        if let Some(StreamState::Response { failed, .. }) = self.streams.remove(&stream) {
            if !failed {
                events.push(H3Event::Failed { stream });
            }
        }
    }
}

/// Extract `:status` from a QPACK field section.
fn parse_status(section: &[u8]) -> Result<u16, qpack::QpackError> {
    let dec = qpack::Decoder::new(section)?;
    for line in dec {
        let line = line?;
        if line.name.as_bytes() == b":status" {
            let v = line.value.decode()?;
            let s = std::str::from_utf8(&v).unwrap_or("0");
            return Ok(s.parse().unwrap_or(0));
        }
    }
    Ok(0) // no status → treat as failure upstream
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for v in [0u64, 1, 63, 64, 16383, 16384, (1 << 30) - 1, 1 << 30, u64::MAX >> 2] {
            let mut buf = Vec::new();
            varint_encode(v, &mut buf);
            let (d, n) = varint_decode(&buf).unwrap();
            assert_eq!(d, v);
            assert_eq!(n, buf.len());
        }
    }

    #[test]
    fn varint_incomplete() {
        assert_eq!(varint_decode(&[]), None);
        assert_eq!(varint_decode(&[0x40]), None); // 2-byte varint, 1 byte given
    }

    #[test]
    fn preamble_structure() {
        let p = client_preamble();
        assert_eq!(p[0], STREAM_TYPE_CONTROL as u8);
        // SETTINGS frame at offset 1
        let (fty, n) = varint_decode(&p[1..]).unwrap();
        assert_eq!(fty, FRAME_SETTINGS);
        let (len, n2) = varint_decode(&p[1 + n..]).unwrap();
        assert_eq!(len as usize, p.len() - 1 - n - n2);
    }

    #[test]
    fn request_get_structure() {
        let mut out = Vec::new();
        encode_request(
            &mut out,
            "GET",
            "dns.google",
            "/dns-query?dns=AAAB",
            None,
            None,
        );
        let (fty, n) = varint_decode(&out).unwrap();
        assert_eq!(fty, FRAME_HEADERS);
        let (len, n2) = varint_decode(&out[n..]).unwrap();
        let block = &out[n + n2..n + n2 + len as usize];
        // Decode the QPACK block and verify pseudo-headers.
        let dec = qpack::Decoder::new(block).unwrap();
        let fields: Vec<(Vec<u8>, Vec<u8>)> = dec
            .map(|l| {
                let l = l.unwrap();
                (l.name.as_bytes().to_vec(), l.value.decode().unwrap())
            })
            .collect();
        assert!(fields.contains(&(b":method".to_vec(), b"GET".to_vec())));
        assert!(fields.contains(&(b":path".to_vec(), b"/dns-query?dns=AAAB".to_vec())));
        assert!(fields.contains(&(b":scheme".to_vec(), b"https".to_vec())));
        assert!(fields.contains(&(b":authority".to_vec(), b"dns.google".to_vec())));
        // No DATA frame for GET.
        assert_eq!(out.len(), n + n2 + len as usize);
    }

    #[test]
    fn request_post_has_data_frame() {
        let mut out = Vec::new();
        encode_request(&mut out, "POST", "d", "/dns-query", None, Some(b"BODY"));
        // Skip HEADERS frame.
        let (_fty, n) = varint_decode(&out).unwrap();
        let (len, n2) = varint_decode(&out[n..]).unwrap();
        let rest = &out[n + n2 + len as usize..];
        let (dty, dn) = varint_decode(rest).unwrap();
        assert_eq!(dty, FRAME_DATA);
        let (dlen, dn2) = varint_decode(&rest[dn..]).unwrap();
        assert_eq!(&rest[dn + dn2..dn + dn2 + dlen as usize], b"BODY");
    }

    #[test]
    fn response_happy_path() {
        let mut h3 = H3::new();
        let stream = 0u64;
        h3.register_request(stream);

        // Build a response: HEADERS(:status 200) + DATA("dns-body")
        let mut block = Vec::new();
        qpack::encode_field_section(&[(":status", "200")], &mut block);
        let mut resp = Vec::new();
        varint_encode(FRAME_HEADERS, &mut resp);
        varint_encode(block.len() as u64, &mut resp);
        resp.extend_from_slice(&block);
        varint_encode(FRAME_DATA, &mut resp);
        varint_encode(8, &mut resp);
        resp.extend_from_slice(b"dns-body");

        let mut ev = Vec::new();
        // Feed in two chunks to test incremental parsing.
        let mid = resp.len() / 2;
        h3.feed(stream, &resp[..mid], &mut ev);
        h3.feed(stream, &resp[mid..], &mut ev);
        assert!(ev.is_empty());
        h3.finish(stream, &mut ev);
        match &ev[0] {
            H3Event::Response { stream: s, body } => {
                assert_eq!(*s, 0);
                assert_eq!(body, b"dns-body");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn response_non_200_fails() {
        let mut h3 = H3::new();
        h3.register_request(4);
        let mut block = Vec::new();
        qpack::encode_field_section(&[(":status", "404")], &mut block);
        let mut resp = Vec::new();
        varint_encode(FRAME_HEADERS, &mut resp);
        varint_encode(block.len() as u64, &mut resp);
        resp.extend_from_slice(&block);
        let mut ev = Vec::new();
        h3.feed(4, &resp, &mut ev);
        h3.finish(4, &mut ev);
        assert!(matches!(ev[0], H3Event::Failed { stream: 4 }));
    }

    #[test]
    fn response_with_indexed_status() {
        // Server sends :status 200 as indexed static entry (0xD9 = 1 1 011001).
        let mut h3 = H3::new();
        h3.register_request(0);
        let mut resp = Vec::new();
        varint_encode(FRAME_HEADERS, &mut resp);
        varint_encode(5, &mut resp); // 2-byte prefix + 3 bytes
        resp.extend_from_slice(&[0x00, 0x00, 0xD9, 0x51, 0x00]); // + name-ref junk value (len 0)
        varint_encode(FRAME_DATA, &mut resp);
        varint_encode(2, &mut resp);
        resp.extend_from_slice(b"ok");
        let mut ev = Vec::new();
        h3.feed(0, &resp, &mut ev);
        h3.finish(0, &mut ev);
        assert!(matches!(&ev[0], H3Event::Response { body, .. } if body == b"ok"));
    }

    #[test]
    fn goaway_on_control_stream() {
        let mut h3 = H3::new();
        let ctrl = 3u64; // server uni stream
        let mut data = Vec::new();
        varint_encode(STREAM_TYPE_CONTROL, &mut data);
        varint_encode(FRAME_GOAWAY, &mut data);
        varint_encode(1, &mut data);
        varint_encode(0, &mut data); // stream id 0
        let mut ev = Vec::new();
        h3.feed(ctrl, &data, &mut ev);
        assert!(h3.goaway_received());
        assert!(matches!(ev[0], H3Event::Goaway));
    }

    #[test]
    fn qpack_streams_are_sunk() {
        let mut h3 = H3::new();
        let enc = 7u64; // server uni
        let mut data = Vec::new();
        varint_encode(0x02, &mut data); // QPACK encoder stream type
        data.extend_from_slice(b"\xde\xad\xbe\xef");
        let mut ev = Vec::new();
        h3.feed(enc, &data, &mut ev);
        h3.feed(enc, b"more bytes", &mut ev);
        assert!(ev.is_empty());
        assert!(!h3.goaway_received());
    }

    #[test]
    fn grease_frames_skipped() {
        let mut h3 = H3::new();
        h3.register_request(0);
        let mut resp = Vec::new();
        // GREASE frame type 0x1f * N + 0x21, e.g. 0x21
        varint_encode(0x21, &mut resp);
        varint_encode(4, &mut resp);
        resp.extend_from_slice(b"junk");
        varint_encode(FRAME_HEADERS, &mut resp);
        let mut block = Vec::new();
        qpack::encode_field_section(&[(":status", "200")], &mut block);
        varint_encode(block.len() as u64, &mut resp);
        resp.extend_from_slice(&block);
        varint_encode(FRAME_DATA, &mut resp);
        varint_encode(3, &mut resp);
        resp.extend_from_slice(b"abc");
        let mut ev = Vec::new();
        h3.feed(0, &resp, &mut ev);
        h3.finish(0, &mut ev);
        assert!(matches!(&ev[0], H3Event::Response { body, .. } if body == b"abc"));
    }

    #[test]
    fn reset_reports_failure() {
        let mut h3 = H3::new();
        h3.register_request(8);
        let mut ev = Vec::new();
        h3.reset(8, &mut ev);
        assert!(matches!(ev[0], H3Event::Failed { stream: 8 }));
    }
}
