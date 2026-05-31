use crate::extractor::{DeltaExtractor, Repair};
use crate::target::{TargetKind, Targets};
use serde_json::Value;

/// OpenAI Chat Completions SSE extractor.
pub struct OpenAi;

impl DeltaExtractor for OpenAi {
    fn on_event(&self, data: &[u8], targets: &mut Targets) {
        if self.is_terminator(data) {
            return;
        }
        let v: Value = match serde_json::from_slice(data) {
            Ok(v) => v,
            Err(_) => return,
        };
        if targets.id.is_none() {
            if let Some(s) = v.get("id").and_then(Value::as_str) {
                targets.id = Some(s.to_string());
            }
        }
        if targets.model.is_none() {
            if let Some(s) = v.get("model").and_then(Value::as_str) {
                targets.model = Some(s.to_string());
            }
        }
        let Some(choices) = v.get("choices").and_then(Value::as_array) else {
            return;
        };
        for choice in choices {
            let ci = choice.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let Some(delta) = choice.get("delta") else { continue };
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                targets.feed(TargetKind::Content { choice: ci }, false, content.as_bytes());
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
                for tc in tcs {
                    let ti = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    if let Some(args) = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                    {
                        targets.feed(TargetKind::ToolArgs { choice: ci, tool: ti }, true, args.as_bytes());
                    }
                }
            }
        }
    }

    fn is_terminator(&self, data: &[u8]) -> bool {
        data.iter()
            .copied()
            .filter(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
            .eq(*b"[DONE]")
    }

    fn synthesize(&self, repairs: &[Repair], targets: &Targets, terminated: bool) -> Vec<u8> {
        // Implemented in Task 5.
        let _ = (repairs, targets, terminated);
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{TargetKind, Targets};

    #[test]
    fn extracts_tool_arguments_fragments() {
        let ext = OpenAi;
        let mut t = Targets::new();
        ext.on_event(
            br#"{"id":"cmpl-1","model":"gpt-4","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":"}}]}}]}"#,
            &mut t,
        );
        ext.on_event(
            br#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"12"}}]}}]}"#,
            &mut t,
        );
        assert_eq!(t.id.as_deref(), Some("cmpl-1"));
        assert_eq!(t.model.as_deref(), Some("gpt-4"));
        let state = t.iter().next().expect("one target");
        assert_eq!(state.kind, TargetKind::ToolArgs { choice: 0, tool: 0 });
        let r = state.repair();
        assert!(r.consistent && r.safe);
        assert_eq!(r.append, b"}");
    }

    #[test]
    fn done_is_terminator() {
        let ext = OpenAi;
        assert!(ext.is_terminator(b"[DONE]"));
        assert!(!ext.is_terminator(br#"{"choices":[]}"#));
    }

    #[test]
    fn plain_text_content_not_repaired() {
        let ext = OpenAi;
        let mut t = Targets::new();
        ext.on_event(br#"{"choices":[{"index":0,"delta":{"content":"Hello, I am"}}]}"#, &mut t);
        let state = t.iter().next().unwrap();
        assert!(!state.repairable(), "prose content must not be repaired");
    }

    #[test]
    fn json_content_is_repaired() {
        let ext = OpenAi;
        let mut t = Targets::new();
        ext.on_event(br#"{"choices":[{"index":0,"delta":{"content":"{\"k\":\"v"}}]}"#, &mut t);
        let state = t.iter().next().unwrap();
        assert!(state.repairable());
        let r = state.repair();
        assert_eq!(r.append, b"\"}");
    }
}
