//! Repair truncated JSON embedded in a complete (non-streaming) JSON response.

use serde_json::Value;

const MAX_DEPTH: usize = 256;

/// Recursively repair embedded truncated-JSON strings in `v`, in place.
/// Returns whether anything changed. `MAX_DEPTH` guards against stack overflow.
pub fn repair_value(v: &mut Value) -> bool {
    repair_value_depth(v, 0)
}

fn repair_value_depth(v: &mut Value, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    match v {
        Value::String(s) => {
            if let Some(fixed) = repair_embedded(s) {
                *s = fixed;
                true
            } else {
                false
            }
        }
        Value::Array(a) => {
            let mut changed = false;
            for e in a.iter_mut() {
                changed |= repair_value_depth(e, depth + 1);
            }
            changed
        }
        Value::Object(m) => {
            let mut changed = false;
            for (_, e) in m.iter_mut() {
                changed |= repair_value_depth(e, depth + 1);
            }
            changed
        }
        _ => false,
    }
}

/// If `s` is JSON-looking (`{`/`[`-leading), currently invalid, and `repair_str` can close
/// it into valid JSON, return the repaired string; otherwise `None` (leave untouched).
fn repair_embedded(s: &str) -> Option<String> {
    let first = s.trim_start().as_bytes().first().copied()?;
    if first != b'{' && first != b'[' {
        return None;
    }
    if serde_json::from_str::<Value>(s).is_ok() {
        return None;
    }
    let repaired = crate::repair_str(s)?;
    if repaired != *s && serde_json::from_str::<Value>(&repaired).is_ok() {
        Some(repaired)
    } else {
        None
    }
}

/// Repair a JSON response: close a truncated *envelope* if needed, then repair embedded
/// truncated-JSON strings. Returns `Some(bytes)` only if something changed; `None` otherwise.
pub fn repair_json_response(body: &[u8]) -> Option<Vec<u8>> {
    let (mut value, envelope_repaired): (Value, bool) = match serde_json::from_slice::<Value>(body)
    {
        Ok(v) => (v, false),
        Err(_) => {
            let s = std::str::from_utf8(body).ok()?;
            let closed = crate::repair_str(s)?;
            (serde_json::from_str(&closed).ok()?, true)
        }
    };
    let nested_repaired = repair_value(&mut value);
    if envelope_repaired || nested_repaired {
        serde_json::to_vec(&value).ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repairs_nested_text_content() {
        let body = br#"{"content":[{"type":"text","text":"{\"chapters\":[{\"title\":\"Intro"}],"stop_reason":"max_tokens"}"#;
        let out = repair_json_response(body).expect("should change");
        let v: Value = serde_json::from_slice(&out).unwrap();
        let text = v["content"][0]["text"].as_str().unwrap();
        serde_json::from_str::<Value>(text).expect("nested text must parse");
        assert_eq!(text, r#"{"chapters":[{"title":"Intro"}]}"#);
    }

    #[test]
    fn repairs_openai_tool_arguments() {
        let body = br#"{"choices":[{"message":{"tool_calls":[{"function":{"arguments":"{\"city\":\"Par"}}]}}]}"#;
        let out = repair_json_response(body).expect("should change");
        let v: Value = serde_json::from_slice(&out).unwrap();
        let args = v["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        assert_eq!(args, r#"{"city":"Par"}"#);
    }

    #[test]
    fn truncated_envelope_is_closed_and_nested_repaired() {
        let body = br#"{"content":[{"type":"text","text":"{\"chapters\":[{\"title\":\"Intro"#;
        let out = repair_json_response(body).expect("should change");
        let v: Value = serde_json::from_slice(&out).expect("envelope now parses");
        let text = v["content"][0]["text"].as_str().unwrap();
        serde_json::from_str::<Value>(text).expect("nested text must parse");
    }

    #[test]
    fn complete_document_is_noop() {
        let body = br#"{"a":1,"b":[2,3],"c":"hello"}"#;
        assert_eq!(repair_json_response(body), None);
    }

    #[test]
    fn prose_content_untouched() {
        let body = br#"{"content":[{"text":"Here is the data:"}]}"#;
        assert_eq!(repair_json_response(body), None);
    }

    #[test]
    fn non_json_passthrough() {
        assert_eq!(repair_json_response(b"not json at all"), None);
    }

    #[test]
    fn preserves_key_order() {
        let body = br#"{"z":1,"a":"{\"x\":1","m":3}"#;
        let out = repair_json_response(body).expect("should change");
        let s = String::from_utf8(out).unwrap();
        let zi = s.find("\"z\"").unwrap();
        let ai = s.find("\"a\"").unwrap();
        let mi = s.find("\"m\"").unwrap();
        assert!(zi < ai && ai < mi, "key order preserved: {s}");
    }

    #[test]
    fn deep_nesting_does_not_panic() {
        let mut body = String::new();
        for _ in 0..2000 {
            body.push('[');
        }
        let _ = repair_json_response(body.as_bytes());
    }
}
