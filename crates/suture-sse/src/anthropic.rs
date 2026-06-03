use crate::extractor::{DeltaExtractor, Repair};
use crate::target::{TargetKind, Targets};
use serde_json::Value;

/// Anthropic Messages SSE extractor.
pub struct Anthropic;

impl DeltaExtractor for Anthropic {
    fn on_event(&self, data: &[u8], targets: &mut Targets) {
        let v: Value = match serde_json::from_slice(data) {
            Ok(v) => v,
            Err(_) => return,
        };
        match v.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(m) = v.get("message") {
                    if let Some(s) = m.get("id").and_then(Value::as_str) {
                        targets.id = Some(s.to_string());
                    }
                    if let Some(s) = m.get("model").and_then(Value::as_str) {
                        targets.model = Some(s.to_string());
                    }
                }
            }
            Some("content_block_delta") => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let Some(delta) = v.get("delta") else { return };
                match delta.get("type").and_then(Value::as_str) {
                    Some("input_json_delta") => {
                        if let Some(pj) = delta.get("partial_json").and_then(Value::as_str) {
                            targets.feed(TargetKind::Block { index: idx }, true, pj.as_bytes());
                        }
                    }
                    Some("text_delta") => {
                        if let Some(txt) = delta.get("text").and_then(Value::as_str) {
                            targets.feed(TargetKind::Block { index: idx }, false, txt.as_bytes());
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn is_terminator(&self, data: &[u8]) -> bool {
        serde_json::from_slice::<Value>(data)
            .ok()
            .and_then(|v| {
                v.get("type")
                    .and_then(Value::as_str)
                    .map(|s| s == "message_stop")
            })
            .unwrap_or(false)
    }

    fn synthesize(&self, repairs: &[Repair], _targets: &Targets, terminated: bool) -> Vec<u8> {
        use crate::extractor::json_escape;
        use crate::target::TargetKind;
        let mut out = String::new();
        for r in repairs {
            let TargetKind::Block { index } = r.kind else {
                continue;
            };
            let esc = json_escape(&r.append);
            out.push_str("event: content_block_delta\n");
            out.push_str(&format!(
                "data: {{\"type\":\"content_block_delta\",\"index\":{index},\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{esc}\"}}}}\n\n"
            ));
            out.push_str("event: content_block_stop\n");
            out.push_str(&format!(
                "data: {{\"type\":\"content_block_stop\",\"index\":{index}}}\n\n"
            ));
        }
        if !terminated {
            out.push_str("event: message_delta\n");
            out.push_str(
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\n\n",
            );
            out.push_str("event: message_stop\n");
            out.push_str("data: {\"type\":\"message_stop\"}\n\n");
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
    fn extracts_input_json_delta() {
        let ext = Anthropic;
        let mut t = Targets::new();
        ext.on_event(
            br#"{"type":"message_start","message":{"id":"msg_1","model":"claude-3"}}"#,
            &mut t,
        );
        ext.on_event(
            br#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use"}}"#,
            &mut t,
        );
        ext.on_event(br#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"x\":1"}}"#, &mut t);
        assert_eq!(t.id.as_deref(), Some("msg_1"));
        assert_eq!(t.model.as_deref(), Some("claude-3"));
        let state = t.iter().next().unwrap();
        assert_eq!(state.kind, TargetKind::Block { index: 0 });
        let r = state.repair();
        assert!(r.consistent && r.safe);
        assert_eq!(r.append, b"}");
    }

    #[test]
    fn plain_text_delta_not_repaired() {
        let ext = Anthropic;
        let mut t = Targets::new();
        ext.on_event(br#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#, &mut t);
        assert!(!t.iter().next().unwrap().repairable());
    }

    #[test]
    fn message_stop_is_terminator() {
        let ext = Anthropic;
        assert!(ext.is_terminator(br#"{"type":"message_stop"}"#));
        assert!(!ext.is_terminator(br#"{"type":"content_block_delta"}"#));
    }
}
