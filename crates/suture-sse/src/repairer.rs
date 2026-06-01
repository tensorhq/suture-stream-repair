use crate::extractor::{DeltaExtractor, Repair};
use crate::sse_parse::SseParser;
use crate::target::Targets;
use bytes::Bytes;

/// Index just past the blank line terminating the first complete SSE event in
/// `buf`, if present. Handles `\n\n`, `\r\n\r\n`, and `\n\r\n`.
fn next_event_boundary(buf: &[u8]) -> Option<usize> {
    let n = buf.len();
    let mut i = 0;
    while i < n {
        if buf[i] == b'\n' {
            if i + 1 < n && buf[i + 1] == b'\n' {
                return Some(i + 2);
            }
            if i + 2 < n && buf[i + 1] == b'\r' && buf[i + 2] == b'\n' {
                return Some(i + 3);
            }
        } else if buf[i] == b'\r'
            && i + 3 < n
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            return Some(i + 4);
        }
        i += 1;
    }
    None
}

/// Drives SSE repair. Forwards COMPLETE, non-terminator events verbatim while
/// holding back (a) the current incomplete event and (b) the terminator event.
/// `finish` emits synthetic repairs THEN the held terminator, and discards the
/// trailing incomplete event (a conformant client discards it too). Repairs are
/// computed only from forwarded events, matching the client's reassembly.
pub struct SseRepairer {
    extractor: Box<dyn DeltaExtractor>,
    targets: Targets,
    /// Un-forwarded bytes: the current incomplete event, and anything after the
    /// terminator once seen.
    buf: Vec<u8>,
    /// Whether the upstream sent its terminator.
    terminated: bool,
    /// Raw bytes of the held terminator event (emitted at finish, after repairs).
    held_terminator: Vec<u8>,
}

impl SseRepairer {
    pub fn new(extractor: Box<dyn DeltaExtractor>) -> Self {
        Self {
            extractor,
            targets: Targets::new(),
            buf: Vec::new(),
            terminated: false,
            held_terminator: Vec::new(),
        }
    }

    /// Extract the data payload of a single complete event's raw bytes.
    fn event_data(bytes: &[u8]) -> Option<Vec<u8>> {
        SseParser::new().push(bytes).into_iter().next()
    }

    /// Forward complete content events verbatim; hold back the terminator and any
    /// trailing incomplete event. Returns the bytes to write downstream.
    pub fn push(&mut self, bytes: &[u8]) -> Bytes {
        self.buf.extend_from_slice(bytes);
        if self.terminated {
            // Hold everything after the terminator (don't forward).
            return Bytes::new();
        }
        let mut forward: Vec<u8> = Vec::new();
        while let Some(end) = next_event_boundary(&self.buf) {
            let event: Vec<u8> = self.buf.drain(..end).collect();
            let data = Self::event_data(&event);
            let is_term = data
                .as_deref()
                .map(|d| self.extractor.is_terminator(d))
                .unwrap_or(false);
            if is_term {
                self.terminated = true;
                self.held_terminator = event;
                break;
            }
            if let Some(d) = data {
                self.extractor.on_event(&d, &mut self.targets);
            }
            forward.extend_from_slice(&event);
        }
        Bytes::from(forward)
    }

    /// Emit synthetic closing events for unbalanced, safe targets, then the held
    /// (or synthesized) terminator. The trailing incomplete event in `buf` is
    /// discarded.
    pub fn finish(&mut self) -> Bytes {
        let mut repairs: Vec<Repair> = Vec::new();
        for state in self.targets.iter() {
            if !state.repairable() {
                continue;
            }
            let ar = state.repair();
            if ar.consistent && ar.safe && !ar.is_noop() {
                repairs.push(Repair { kind: state.kind.clone(), append: ar.append });
            }
        }
        // `synthesize` emits a terminator only when `terminated` is false.
        let mut out = self.extractor.synthesize(&repairs, &self.targets, self.terminated);
        out.extend_from_slice(&self.held_terminator);
        Bytes::from(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Anthropic, OpenAi};
    use serde_json::Value;

    fn reassemble_openai_args(sse: &[u8]) -> String {
        let mut parser = crate::SseParser::new();
        let mut args = String::new();
        for data in parser.push(sse) {
            if data == b"[DONE]" {
                continue;
            }
            let v: Value = match serde_json::from_slice(&data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(tcs) = v["choices"][0]["delta"]["tool_calls"].as_array() {
                for tc in tcs {
                    if let Some(a) = tc["function"]["arguments"].as_str() {
                        args.push_str(a);
                    }
                }
            }
        }
        args
    }

    #[test]
    fn openai_truncated_tool_args_repaired_end_to_end() {
        let upstream = "data: {\"id\":\"c1\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\\\"Par\"}}]}}]}\n\n";
        let mut r = SseRepairer::new(Box::new(OpenAi));
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&r.push(upstream.as_bytes()));
        out.extend_from_slice(&r.finish());

        let args = reassemble_openai_args(&out);
        assert_eq!(args, r#"{"city":"Par"}"#);
        serde_json::from_str::<Value>(&args).expect("repaired args must parse");
        assert!(out.starts_with(upstream.as_bytes()));
        assert!(out.windows(6).any(|w| w == b"[DONE]"));
    }

    #[test]
    fn openai_complete_stream_is_unchanged() {
        let upstream = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut r = SseRepairer::new(Box::new(OpenAi));
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&r.push(upstream.as_bytes()));
        out.extend_from_slice(&r.finish());
        assert_eq!(out, upstream.as_bytes());
    }

    #[test]
    fn openai_mid_event_death_no_corruption_and_valid() {
        // event 1 complete (args fragment `{"city":"Par`), event 2 truncated mid-envelope (no blank line)
        let ev1 = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\\\"Par\"}}]}}]}\n\n";
        let ev2_partial = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"is";
        let mut r = SseRepairer::new(Box::new(OpenAi));
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&r.push(ev1.as_bytes()));
        out.extend_from_slice(&r.push(ev2_partial.as_bytes()));
        out.extend_from_slice(&r.finish());

        // The truncated partial event must NOT have been forwarded (no corruption).
        assert!(!out.windows(2).any(|w| w == b"is"), "partial event bytes leaked: {}", String::from_utf8_lossy(&out));
        // Reassembly is valid JSON.
        let args = reassemble_openai_args(&out);
        assert_eq!(args, r#"{"city":"Par"}"#);
        serde_json::from_str::<Value>(&args).expect("must parse");
    }

    #[test]
    fn openai_repair_emitted_before_done() {
        // truncated args event, THEN upstream [DONE]
        let upstream = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"k\\\":\\\"v\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut r = SseRepairer::new(Box::new(OpenAi));
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&r.push(upstream.as_bytes()));
        out.extend_from_slice(&r.finish());

        let s = String::from_utf8(out.clone()).unwrap();
        let done_pos = s.find("[DONE]").expect("has DONE");
        let repair_pos = s.find("chat.completion.chunk").expect("has a synthetic repair chunk");
        assert!(repair_pos < done_pos, "repair must come BEFORE [DONE]\n{s}");
        // and reassembly is valid
        let args = reassemble_openai_args(out.as_slice());
        assert_eq!(args, r#"{"k":"v"}"#);
        serde_json::from_str::<Value>(&args).unwrap();
    }

    #[test]
    fn anthropic_truncated_tool_input_repaired_end_to_end() {
        let upstream = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":[1,2\"}}\n\n",
        );
        let mut r = SseRepairer::new(Box::new(Anthropic));
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&r.push(upstream.as_bytes()));
        out.extend_from_slice(&r.finish());

        let mut parser = crate::SseParser::new();
        let mut pj = String::new();
        for data in parser.push(&out) {
            if let Ok(v) = serde_json::from_slice::<Value>(&data) {
                if v["delta"]["type"] == "input_json_delta" {
                    if let Some(s) = v["delta"]["partial_json"].as_str() {
                        pj.push_str(s);
                    }
                }
            }
        }
        assert_eq!(pj, r#"{"a":[1,2]}"#);
        serde_json::from_str::<Value>(&pj).expect("repaired input must parse");
    }
}
