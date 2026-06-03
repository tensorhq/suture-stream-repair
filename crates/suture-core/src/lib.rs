//! Suture core JSON repair engine.
//!
//! Targets JSON whose top-level value is an object or array. Truncated bare
//! top-level scalars are out of scope and returned unchanged.

mod repair;

pub use repair::{AppendRepair, Repair, StreamRepairer};

/// Repair a complete (possibly truncated) JSON string.
///
/// Returns `Some(valid_json)` when the input is structurally consistent, or
/// `None` when it is not (caller should pass the original through untouched).
pub fn repair_str(input: &str) -> Option<String> {
    let mut r = StreamRepairer::new();
    r.push(input.as_bytes());
    let rep = r.finish();
    if !rep.consistent {
        return None;
    }
    let keep = input.len() - rep.drop_trailing;
    let mut out = input.as_bytes()[..keep].to_vec();
    out.extend_from_slice(&rep.append);
    // If the repair somehow left invalid UTF-8 (only reachable via malformed input on the
    // raw byte API), report inconsistency rather than panicking.
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use proptest::prelude::*;

    fn corpus() -> Vec<&'static str> {
        vec![
            r#"{}"#,
            r#"[]"#,
            r#"{"a":1}"#,
            r#"{"id":42,"generation":"The application sequence failed"}"#,
            r#"{"status":"partial","payload_metrics":[250,194,7]}"#,
            r#"[true,false,null,3.14,-2e10]"#,
            r#"{"nested":{"a":["x","y"],"b":{"c":1}}}"#,
            r#"{"s":"with \"quotes\" and \\ and é unicode"}"#,
            r#"{"tools":[{"name":"f","arguments":"{\"x\":1}"}]}"#,
            r#"[{"k":[1,[2,[3,[]]]]}]"#,
        ]
    }

    #[test]
    fn empty_input_is_noop() {
        assert_eq!(repair_str(""), Some(String::new()));
    }

    #[test]
    fn complete_valid_json_is_noop() {
        for s in corpus() {
            let mut r = StreamRepairer::new();
            r.push(s.as_bytes());
            let rep = r.finish();
            assert!(rep.consistent, "should be consistent: {s}");
            assert!(rep.is_noop(), "complete JSON must be a no-op: {s}");
        }
    }

    #[test]
    fn every_prefix_repairs_to_parseable_json() {
        for s in corpus() {
            let bytes = s.as_bytes();
            for cut in 1..=bytes.len() {
                // Only test prefixes that end on a UTF-8 char boundary.
                if !s.is_char_boundary(cut) {
                    continue;
                }
                let prefix = &s[..cut];
                if let Some(repaired) = repair_str(prefix) {
                    serde_json::from_str::<Value>(&repaired).unwrap_or_else(|e| {
                        panic!("prefix {prefix:?} -> {repaired:?} did not parse: {e}")
                    });
                }
                // A `None` result (inconsistent) is acceptable: caller passes through.
            }
        }
    }

    proptest! {
        /// The core invariant: for any input that begins with `{` or `[`, if `repair_str`
        /// reports success, the output MUST parse as JSON. The alphabet deliberately includes
        /// control chars, backslash escapes, and structural punctuation.
        #[test]
        fn repaired_container_input_always_parses(s in r#"[\{\[]([a-z0-9 "\\/:,\{\}\[\]\t\n-]{0,40})"#) {
            if let Some(out) = repair_str(&s) {
                prop_assert!(
                    serde_json::from_str::<serde_json::Value>(&out).is_ok(),
                    "input {:?} -> {:?} did not parse", s, out
                );
            }
        }
    }

    #[test]
    fn chunking_does_not_change_result() {
        let s = r#"{"a":["x",{"b":"c"#;
        let whole = {
            let mut r = StreamRepairer::new();
            r.push(s.as_bytes());
            r.finish()
        };
        let chunked = {
            let mut r = StreamRepairer::new();
            for byte in s.as_bytes() {
                r.push(std::slice::from_ref(byte));
            }
            r.finish()
        };
        assert_eq!(whole, chunked);
    }
}
