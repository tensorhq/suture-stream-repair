//! Incremental codec for AWS `application/vnd.amazon.eventstream` binary frames.

/// A parsed eventstream frame. Only string-typed headers are retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub headers: Vec<(String, String)>,
    pub payload: Vec<u8>,
    /// The full raw frame bytes (for verbatim forwarding).
    pub raw: Vec<u8>,
}

impl Frame {
    /// The `:event-type` header value, if present.
    pub fn event_type(&self) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == ":event-type").map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// A prelude or message CRC did not validate.
    Crc,
    /// Structurally malformed (bad lengths or header encoding).
    Malformed,
}

const MAX_FRAME: usize = 16 * 1024 * 1024;

fn crc32(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

/// Try to parse one frame from the front of `buf`.
/// `Ok(Some((frame, consumed)))` — a complete frame; `Ok(None)` — need more bytes;
/// `Err(..)` — a CRC failure or malformed frame.
pub fn parse_frame(buf: &[u8]) -> Result<Option<(Frame, usize)>, FrameError> {
    if buf.len() < 12 {
        return Ok(None);
    }
    let total_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let headers_len = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    let prelude_crc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
    if crc32(&buf[..8]) != prelude_crc {
        return Err(FrameError::Crc);
    }
    if total_len < headers_len + 16 || total_len > MAX_FRAME {
        return Err(FrameError::Malformed);
    }
    if buf.len() < total_len {
        return Ok(None);
    }
    let frame = &buf[..total_len];
    let msg_crc = u32::from_be_bytes([
        frame[total_len - 4],
        frame[total_len - 3],
        frame[total_len - 2],
        frame[total_len - 1],
    ]);
    if crc32(&frame[..total_len - 4]) != msg_crc {
        return Err(FrameError::Crc);
    }
    let headers = parse_headers(&frame[12..12 + headers_len])?;
    let payload = frame[12 + headers_len..total_len - 4].to_vec();
    Ok(Some((Frame { headers, payload, raw: frame.to_vec() }, total_len)))
}

fn parse_headers(mut b: &[u8]) -> Result<Vec<(String, String)>, FrameError> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let name_len = b[0] as usize;
        b = &b[1..];
        if b.len() < name_len + 1 {
            return Err(FrameError::Malformed);
        }
        let name = std::str::from_utf8(&b[..name_len]).map_err(|_| FrameError::Malformed)?.to_string();
        b = &b[name_len..];
        let vtype = b[0];
        b = &b[1..];
        let val_bytes = match vtype {
            0 | 1 => 0,
            2 => 1,
            3 => 2,
            4 => 4,
            5 | 8 => 8,
            9 => 16,
            6 | 7 => {
                if b.len() < 2 {
                    return Err(FrameError::Malformed);
                }
                let vlen = u16::from_be_bytes([b[0], b[1]]) as usize;
                b = &b[2..];
                if b.len() < vlen {
                    return Err(FrameError::Malformed);
                }
                if vtype == 7 {
                    let value = std::str::from_utf8(&b[..vlen]).map_err(|_| FrameError::Malformed)?.to_string();
                    out.push((name, value));
                }
                b = &b[vlen..];
                continue;
            }
            _ => return Err(FrameError::Malformed),
        };
        if b.len() < val_bytes {
            return Err(FrameError::Malformed);
        }
        b = &b[val_bytes..];
    }
    Ok(out)
}

/// Build a complete frame from string headers and a payload (computes both CRC32s).
pub fn build_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut hb = Vec::new();
    for (name, val) in headers {
        hb.push(name.len() as u8);
        hb.extend_from_slice(name.as_bytes());
        hb.push(7u8);
        hb.extend_from_slice(&(val.len() as u16).to_be_bytes());
        hb.extend_from_slice(val.as_bytes());
    }
    let headers_len = hb.len() as u32;
    let total_len = 12 + headers_len + payload.len() as u32 + 4;

    let mut msg = Vec::with_capacity(total_len as usize);
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&headers_len.to_be_bytes());
    let prelude_crc = crc32(&msg);
    msg.extend_from_slice(&prelude_crc.to_be_bytes());
    msg.extend_from_slice(&hb);
    msg.extend_from_slice(payload);
    let msg_crc = crc32(&msg);
    msg.extend_from_slice(&msg_crc.to_be_bytes());
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_then_parse_round_trips() {
        let raw = build_frame(
            &[(":event-type", "contentBlockDelta"), (":content-type", "application/json")],
            br#"{"x":1}"#,
        );
        let (frame, consumed) = parse_frame(&raw).unwrap().expect("a complete frame");
        assert_eq!(consumed, raw.len());
        assert_eq!(frame.event_type(), Some("contentBlockDelta"));
        assert_eq!(frame.payload, br#"{"x":1}"#);
        assert_eq!(frame.raw, raw);
    }

    #[test]
    fn need_more_bytes_returns_none() {
        let raw = build_frame(&[(":event-type", "messageStop")], b"{}");
        assert_eq!(parse_frame(&raw[..raw.len() - 3]).unwrap(), None);
        assert_eq!(parse_frame(&raw[..4]).unwrap(), None);
    }

    #[test]
    fn message_crc_mismatch_is_err() {
        let mut raw = build_frame(&[(":event-type", "x")], b"payload");
        let n = raw.len();
        raw[n - 6] ^= 0xFF;
        assert_eq!(parse_frame(&raw), Err(FrameError::Crc));
    }

    #[test]
    fn prelude_crc_mismatch_is_err() {
        let mut raw = build_frame(&[(":event-type", "x")], b"p");
        raw[2] ^= 0xFF;
        assert_eq!(parse_frame(&raw), Err(FrameError::Crc));
    }

    #[test]
    fn two_frames_parse_sequentially() {
        let mut buf = build_frame(&[(":event-type", "a")], b"1");
        let second = build_frame(&[(":event-type", "b")], b"2");
        buf.extend_from_slice(&second);
        let (f1, c1) = parse_frame(&buf).unwrap().unwrap();
        assert_eq!(f1.event_type(), Some("a"));
        let (f2, c2) = parse_frame(&buf[c1..]).unwrap().unwrap();
        assert_eq!(f2.event_type(), Some("b"));
        assert_eq!(c1 + c2, buf.len());
    }
}
