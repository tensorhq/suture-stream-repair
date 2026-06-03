use crate::eventstream::{build_frame, parse_frame, Frame};
use crate::extractor::json_escape;
use crate::target::{TargetKind, Targets};
use bytes::Bytes;
use serde_json::Value;

fn extract(frame: &Frame, targets: &mut Targets) {
    if frame.event_type() != Some("contentBlockDelta") {
        return;
    }
    let v: Value = match serde_json::from_slice(&frame.payload) {
        Ok(v) => v,
        Err(_) => return,
    };
    let idx = v
        .get("contentBlockIndex")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let Some(delta) = v.get("delta") else { return };
    if let Some(input) = delta
        .get("toolUse")
        .and_then(|t| t.get("input"))
        .and_then(Value::as_str)
    {
        targets.feed(TargetKind::Block { index: idx }, true, input.as_bytes());
    } else if let Some(text) = delta.get("text").and_then(Value::as_str) {
        targets.feed(TargetKind::Block { index: idx }, false, text.as_bytes());
    }
}

fn is_terminator(frame: &Frame) -> bool {
    frame.event_type() == Some("messageStop")
}

fn synth_repair_frame(index: usize, append: &[u8]) -> Vec<u8> {
    let esc = json_escape(append);
    let payload = format!(
        "{{\"contentBlockIndex\":{index},\"delta\":{{\"toolUse\":{{\"input\":\"{esc}\"}}}}}}"
    );
    build_frame(
        &[
            (":event-type", "contentBlockDelta"),
            (":content-type", "application/json"),
            (":message-type", "event"),
        ],
        payload.as_bytes(),
    )
}

/// Repairs AWS eventstream (Converse) tool-input JSON. Forwards complete non-terminator
/// frames verbatim; holds back the incomplete trailing frame and the `messageStop` frame;
/// on `finish` synthesizes a repair frame before the held terminator.
pub struct EventStreamRepairer {
    targets: Targets,
    buf: Vec<u8>,
    terminated: bool,
    held_terminator: Vec<u8>,
    consistent: bool,
}

impl Default for EventStreamRepairer {
    fn default() -> Self {
        Self::new()
    }
}

impl EventStreamRepairer {
    pub fn new() -> Self {
        Self {
            targets: Targets::new(),
            buf: Vec::new(),
            terminated: false,
            held_terminator: Vec::new(),
            consistent: true,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Bytes {
        if !self.consistent {
            return Bytes::copy_from_slice(bytes);
        }
        self.buf.extend_from_slice(bytes);
        if self.terminated {
            return Bytes::new();
        }
        let mut forward: Vec<u8> = Vec::new();
        loop {
            match parse_frame(&self.buf) {
                Ok(Some((frame, consumed))) => {
                    let raw: Vec<u8> = self.buf.drain(..consumed).collect();
                    if is_terminator(&frame) {
                        self.terminated = true;
                        self.held_terminator = raw;
                        break;
                    }
                    extract(&frame, &mut self.targets);
                    forward.extend_from_slice(&raw);
                }
                Ok(None) => break,
                Err(_) => {
                    self.consistent = false;
                    forward.extend_from_slice(&self.buf);
                    self.buf.clear();
                    break;
                }
            }
        }
        Bytes::from(forward)
    }

    pub fn finish(&mut self) -> Bytes {
        if !self.consistent {
            return Bytes::from(std::mem::take(&mut self.held_terminator));
        }
        let mut out: Vec<u8> = Vec::new();
        for state in self.targets.iter() {
            if !state.repairable() {
                continue;
            }
            let ar = state.repair();
            if ar.consistent && ar.safe && !ar.is_noop() {
                if let TargetKind::Block { index } = &state.kind {
                    out.extend_from_slice(&synth_repair_frame(*index, &ar.append));
                }
            }
        }
        out.extend_from_slice(&self.held_terminator);
        Bytes::from(out)
    }
}

/// Wrap an upstream eventstream byte stream, repairing truncated Converse tool input.
pub fn eventstream_repair_stream<S, E>(
    upstream: S,
) -> impl futures_core::Stream<Item = Result<Bytes, E>>
where
    S: futures_core::Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    use futures_util::StreamExt;
    let repairer = EventStreamRepairer::new();
    futures_util::stream::unfold(
        (upstream.boxed(), repairer, false),
        |(mut up, mut repairer, finished)| async move {
            if finished {
                return None;
            }
            match up.next().await {
                Some(Ok(chunk)) => {
                    let forwarded = repairer.push(&chunk);
                    Some((Ok(forwarded), (up, repairer, false)))
                }
                Some(Err(e)) => Some((Err(e), (up, repairer, true))),
                None => {
                    let tail = repairer.finish();
                    if tail.is_empty() {
                        None
                    } else {
                        Some((Ok(tail), (up, repairer, true)))
                    }
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eventstream::{build_frame, parse_frame};

    fn delta_frame(idx: u64, input: &str) -> Vec<u8> {
        let payload =
            serde_json::json!({"contentBlockIndex": idx, "delta": {"toolUse": {"input": input}}})
                .to_string();
        build_frame(
            &[
                (":event-type", "contentBlockDelta"),
                (":content-type", "application/json"),
                (":message-type", "event"),
            ],
            payload.as_bytes(),
        )
    }
    fn message_stop() -> Vec<u8> {
        build_frame(
            &[(":event-type", "messageStop")],
            br#"{"stopReason":"max_tokens"}"#,
        )
    }
    fn reassemble(bytes: &[u8]) -> String {
        let mut out = String::new();
        let mut off = 0;
        while let Ok(Some((frame, consumed))) = parse_frame(&bytes[off..]) {
            if frame.event_type() == Some("contentBlockDelta") {
                let v: serde_json::Value = serde_json::from_slice(&frame.payload).unwrap();
                if let Some(s) = v["delta"]["toolUse"]["input"].as_str() {
                    out.push_str(s);
                }
            }
            off += consumed;
            if off >= bytes.len() {
                break;
            }
        }
        out
    }

    #[test]
    fn repairs_truncated_tool_input() {
        let mut r = EventStreamRepairer::new();
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&r.push(&delta_frame(0, r#"{"city":"Par"#)));
        out.extend_from_slice(&r.finish());
        let input = reassemble(&out);
        assert_eq!(input, r#"{"city":"Par"}"#);
        serde_json::from_str::<serde_json::Value>(&input).expect("repaired input must parse");
    }

    #[test]
    fn repair_emitted_before_held_message_stop() {
        let mut r = EventStreamRepairer::new();
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&r.push(&delta_frame(0, r#"{"a":[1,2"#)));
        out.extend_from_slice(&r.push(&message_stop()));
        out.extend_from_slice(&r.finish());
        let (f1, c1) = parse_frame(&out).unwrap().unwrap();
        assert_eq!(f1.event_type(), Some("contentBlockDelta"));
        let (f2, c2) = parse_frame(&out[c1..]).unwrap().unwrap();
        assert_eq!(
            f2.event_type(),
            Some("contentBlockDelta"),
            "synthetic repair frame"
        );
        let (f3, _) = parse_frame(&out[c1 + c2..]).unwrap().unwrap();
        assert_eq!(f3.event_type(), Some("messageStop"));
        assert_eq!(reassemble(&out), r#"{"a":[1,2]}"#);
    }

    #[test]
    fn complete_balanced_stream_unchanged() {
        let mut r = EventStreamRepairer::new();
        let input = {
            let mut b = delta_frame(0, "{}");
            b.extend_from_slice(&message_stop());
            b
        };
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&r.push(&input));
        out.extend_from_slice(&r.finish());
        assert_eq!(out, input, "balanced stream passes through unchanged");
    }

    #[test]
    fn mid_frame_death_no_leak() {
        let mut r = EventStreamRepairer::new();
        let complete = delta_frame(0, r#"{"city":"Par"#);
        let partial = &delta_frame(0, "ignored")[..10];
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&r.push(&complete));
        out.extend_from_slice(&r.push(partial));
        out.extend_from_slice(&r.finish());
        assert_eq!(reassemble(&out), r#"{"city":"Par"}"#);
    }
}
