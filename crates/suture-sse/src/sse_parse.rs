/// Incremental Server-Sent-Events parser. Feed arbitrary byte chunks; returns the
/// `data` payload of each fully-received event (multiple `data:` lines joined by
/// `\n`). Comment lines (starting with `:`) and non-`data` fields are ignored.
pub struct SseParser {
    /// Bytes not yet forming a complete line.
    buf: Vec<u8>,
    /// Accumulated `data` field for the event currently being assembled.
    data: Vec<u8>,
    /// Whether the current event has had at least one `data:` line.
    saw_data: bool,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            data: Vec::new(),
            saw_data: false,
        }
    }

    /// Feed a chunk; returns the data payloads of any events completed by it.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop(); // remove '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                if self.saw_data {
                    events.push(std::mem::take(&mut self.data));
                    self.saw_data = false;
                } else {
                    self.data.clear();
                }
                continue;
            }
            if line[0] == b':' {
                continue; // comment
            }
            let (field, value) = match line.iter().position(|&b| b == b':') {
                Some(i) => {
                    let mut v = &line[i + 1..];
                    if v.first() == Some(&b' ') {
                        v = &v[1..];
                    }
                    (&line[..i], v.to_vec())
                }
                None => (&line[..], Vec::new()),
            };
            if field == b"data" {
                if self.saw_data {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(&value);
                self.saw_data = true;
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payloads(parser: &mut SseParser, chunk: &[u8]) -> Vec<String> {
        parser
            .push(chunk)
            .into_iter()
            .map(|p| String::from_utf8(p).unwrap())
            .collect()
    }

    #[test]
    fn single_event() {
        let mut p = SseParser::new();
        let out = payloads(&mut p, b"data: {\"x\":1}\n\n");
        assert_eq!(out, vec![r#"{"x":1}"#]);
    }

    #[test]
    fn split_across_chunks() {
        let mut p = SseParser::new();
        assert!(payloads(&mut p, b"data: {\"x\"").is_empty());
        assert!(payloads(&mut p, b":1}").is_empty());
        let out = payloads(&mut p, b"\n\n");
        assert_eq!(out, vec![r#"{"x":1}"#]);
    }

    #[test]
    fn crlf_and_comments_and_keepalive() {
        let mut p = SseParser::new();
        let out = payloads(&mut p, b": keep-alive\r\ndata: hello\r\n\r\n");
        assert_eq!(out, vec!["hello"]);
    }

    #[test]
    fn multiple_data_lines_joined_with_newline() {
        let mut p = SseParser::new();
        let out = payloads(&mut p, b"data: a\ndata: b\n\n");
        assert_eq!(out, vec!["a\nb"]);
    }

    #[test]
    fn two_events_one_chunk() {
        let mut p = SseParser::new();
        let out = payloads(&mut p, b"data: [DONE]\n\ndata: x\n\n");
        assert_eq!(out, vec!["[DONE]", "x"]);
    }

    #[test]
    fn no_leading_space_after_colon() {
        let mut p = SseParser::new();
        let out = payloads(&mut p, b"data:tight\n\n");
        assert_eq!(out, vec!["tight"]);
    }
}
