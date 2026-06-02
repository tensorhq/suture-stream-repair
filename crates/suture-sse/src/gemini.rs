use crate::extractor::{json_escape, DeltaExtractor, Repair};
use crate::target::{TargetKind, Targets};
use serde_json::Value;

/// Vertex AI Gemini SSE extractor (`streamGenerateContent?alt=sse`). Repairs
/// reassembled `candidates[i].content.parts[].text` when it is JSON-looking
/// (JSON-mode output). `functionCall` parts arrive whole and are ignored.
pub struct Gemini;

impl DeltaExtractor for Gemini {
    fn on_event(&self, data: &[u8], targets: &mut Targets) {
        let v: Value = match serde_json::from_slice(data) {
            Ok(v) => v,
            Err(_) => return,
        };
        let Some(cands) = v.get("candidates").and_then(Value::as_array) else {
            return;
        };
        for cand in cands {
            let idx = cand.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let Some(parts) = cand
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    targets.feed(TargetKind::Content { choice: idx }, false, text.as_bytes());
                }
            }
        }
    }

    fn is_terminator(&self, _data: &[u8]) -> bool {
        false
    }

    fn synthesize(&self, repairs: &[Repair], _targets: &Targets, _terminated: bool) -> Vec<u8> {
        let mut out = String::new();
        for r in repairs {
            let choice = if let TargetKind::Content { choice } = &r.kind {
                *choice
            } else {
                continue;
            };
            let esc = json_escape(&r.append);
            out.push_str(&format!(
                "data: {{\"candidates\":[{{\"index\":{choice},\"content\":{{\"parts\":[{{\"text\":\"{esc}\"}}]}},\"finishReason\":\"length\"}}]}}\n\n"
            ));
        }
        out.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extractor::DeltaExtractor;
    use crate::target::{TargetKind, Targets};

    #[test]
    fn extracts_json_text_parts() {
        let ext = Gemini;
        let mut t = Targets::new();
        ext.on_event(
            br#"{"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"{\"city\":"}]}}]}"#,
            &mut t,
        );
        ext.on_event(
            br#"{"candidates":[{"index":0,"content":{"parts":[{"text":"\"Par"}]}}]}"#,
            &mut t,
        );
        let state = t.iter().next().expect("one target");
        assert_eq!(state.kind, TargetKind::Content { choice: 0 });
        assert!(state.repairable(), "json-looking text must be repairable");
        let r = state.repair();
        assert!(r.consistent && r.safe);
        assert_eq!(r.append, b"\"}");
    }

    #[test]
    fn plain_text_not_repaired() {
        let ext = Gemini;
        let mut t = Targets::new();
        ext.on_event(
            br#"{"candidates":[{"index":0,"content":{"parts":[{"text":"Hello there"}]}}]}"#,
            &mut t,
        );
        assert!(!t.iter().next().unwrap().repairable());
    }

    #[test]
    fn function_call_part_is_ignored() {
        let ext = Gemini;
        let mut t = Targets::new();
        ext.on_event(
            br#"{"candidates":[{"index":0,"content":{"parts":[{"functionCall":{"name":"f","args":{"x":1}}}]}}]}"#,
            &mut t,
        );
        assert!(t.iter().next().is_none(), "functionCall parts create no target");
    }

    #[test]
    fn never_terminator() {
        let ext = Gemini;
        assert!(!ext.is_terminator(br#"{"candidates":[{"finishReason":"STOP"}]}"#));
    }

    #[test]
    fn multi_candidate_indexing() {
        let ext = Gemini;
        let mut t = Targets::new();
        ext.on_event(
            br#"{"candidates":[{"index":0,"content":{"parts":[{"text":"["}]}},{"index":1,"content":{"parts":[{"text":"{"}]}}]}"#,
            &mut t,
        );
        let kinds: Vec<_> = t.iter().map(|s| s.kind.clone()).collect();
        assert!(kinds.contains(&TargetKind::Content { choice: 0 }));
        assert!(kinds.contains(&TargetKind::Content { choice: 1 }));
    }
}
