use crate::extractor::{DeltaExtractor, Repair};
use crate::sse_parse::SseParser;
use crate::target::Targets;
use bytes::Bytes;

/// Drives SSE repair: forwards upstream bytes verbatim while tracking reassembled
/// targets; on `finish` synthesizes closing events for unbalanced, safe targets.
pub struct SseRepairer {
    parser: SseParser,
    extractor: Box<dyn DeltaExtractor>,
    targets: Targets,
    terminated: bool,
}

impl SseRepairer {
    pub fn new(extractor: Box<dyn DeltaExtractor>) -> Self {
        Self {
            parser: SseParser::new(),
            extractor,
            targets: Targets::new(),
            terminated: false,
        }
    }

    /// Forward `bytes` downstream unchanged, updating tracking.
    pub fn push(&mut self, bytes: &[u8]) -> Bytes {
        for data in self.parser.push(bytes) {
            if self.extractor.is_terminator(&data) {
                self.terminated = true;
            }
            self.extractor.on_event(&data, &mut self.targets);
        }
        Bytes::copy_from_slice(bytes)
    }

    /// Emit synthetic closing events for any unbalanced, safe target.
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
        if repairs.is_empty() && self.terminated {
            return Bytes::new();
        }
        let bytes = self.extractor.synthesize(&repairs, &self.targets, self.terminated);
        Bytes::from(bytes)
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
