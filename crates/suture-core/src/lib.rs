//! Suture core JSON repair engine.
//!
//! Targets JSON whose top-level value is an object or array. Truncated bare
//! top-level scalars are out of scope and returned unchanged.

mod repair;

pub use repair::{Repair, StreamRepairer};

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
    Some(String::from_utf8(out).expect("repair lands on UTF-8 boundaries"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_noop() {
        assert_eq!(repair_str(""), Some(String::new()));
    }

    #[test]
    fn already_valid_object_unchanged() {
        // With the stub, an already-valid object round-trips unchanged.
        assert_eq!(repair_str("{}"), Some("{}".to_string()));
    }
}
