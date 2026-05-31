/// Result of an append-only repair (for streaming passthrough, where already-
/// emitted bytes cannot be retracted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendRepair {
    /// Structurally consistent so far. If false, pass the original through untouched.
    pub consistent: bool,
    /// True if appending `append` to the already-emitted bytes yields valid JSON
    /// WITHOUT dropping anything. False means append-only cannot fix the tail
    /// (trailing comma, partial scalar/keyword, mid-escape, incomplete key, or a
    /// truncated multibyte UTF-8 char) — the caller should skip this target.
    pub safe: bool,
    /// Bytes to append when `safe` (optional closing '"' then container closers).
    pub append: Vec<u8>,
}

impl AppendRepair {
    pub fn is_noop(&self) -> bool {
        self.append.is_empty()
    }
}

/// Result of computing how to make a (possibly truncated) JSON byte stream valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    pub consistent: bool,
    pub drop_trailing: usize,
    pub append: Vec<u8>,
}

impl Repair {
    pub fn is_noop(&self) -> bool {
        self.drop_trailing == 0 && self.append.is_empty()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Frame {
    Object,
    Array,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pos {
    TopBefore,
    TopAfter,
    ArrBeforeElem,
    ArrAfterElem,
    ObjBeforeKey,
    ObjAfterKey,
    ObjBeforeVal,
    ObjAfterVal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lex {
    Between,
    Str,
    StrEsc,
    StrU(u8),
    Scalar,
}

struct FrameState {
    frame: Frame,
    pos: Pos,
    /// Offset to drop back to in order to remove the current (incomplete)
    /// element/member INCLUDING any preceding comma. Updated when entering a
    /// "before key/elem" state (just after `{`/`[`/`,`).
    elem_drop_to: usize,
    /// Whether this container has had at least one complete element/member.
    seen_member: bool,
}

pub struct StreamRepairer {
    frames: Vec<FrameState>,
    top_pos: Pos,
    lex: Lex,
    consistent: bool,
    len: usize,
    /// Whether the current/just-finished string sits in an object-key position.
    str_is_key: bool,
    /// Raw bytes of the current scalar token (for shape validation).
    scalar_buf: Vec<u8>,
    /// Count of trailing bytes belonging to an as-yet-incomplete multibyte
    /// UTF-8 char inside the current string (0 when on a char boundary).
    str_incomplete: usize,
    /// Total expected byte length of the current multibyte char.
    str_char_len: usize,
}

impl Default for StreamRepairer {
    fn default() -> Self {
        Self::new()
    }
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

/// Validate a complete JSON number per RFC 8259 grammar:
/// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`
fn is_valid_json_number(b: &[u8]) -> bool {
    let n = b.len();
    let mut i = 0;
    if i < n && b[i] == b'-' {
        i += 1;
    }
    if i >= n {
        return false;
    }
    if b[i] == b'0' {
        i += 1;
    } else if b[i].is_ascii_digit() {
        while i < n && b[i].is_ascii_digit() {
            i += 1;
        }
    } else {
        return false;
    }
    if i < n && b[i] == b'.' {
        i += 1;
        if i >= n || !b[i].is_ascii_digit() {
            return false;
        }
        while i < n && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < n && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < n && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        if i >= n || !b[i].is_ascii_digit() {
            return false;
        }
        while i < n && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    i == n
}

fn is_valid_scalar(b: &[u8]) -> bool {
    matches!(b, b"true" | b"false" | b"null") || is_valid_json_number(b)
}

fn is_scalar_start(b: u8) -> bool {
    b.is_ascii_digit() || matches!(b, b'-' | b't' | b'f' | b'n')
}

fn is_scalar_byte(b: u8) -> bool {
    b.is_ascii_digit()
        || matches!(
            b,
            b'-' | b'+' | b'.' | b'e' | b'E'
                | b't' | b'r' | b'u' | b'f' | b'a' | b'l' | b's' | b'n'
        )
}

impl StreamRepairer {
    pub fn new() -> Self {
        Self {
            frames: Vec::new(),
            top_pos: Pos::TopBefore,
            lex: Lex::Between,
            consistent: true,
            len: 0,
            str_is_key: false,
            scalar_buf: Vec::new(),
            str_incomplete: 0,
            str_char_len: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.process(b);
        }
    }

    /// Track multibyte UTF-8 progress for raw bytes inside a string.
    fn track_utf8(&mut self, b: u8) {
        if b < 0x80 {
            self.str_incomplete = 0;
        } else if b >= 0xC0 {
            // lead byte
            self.str_char_len = if b >= 0xF0 {
                4
            } else if b >= 0xE0 {
                3
            } else {
                2
            };
            self.str_incomplete = 1;
        } else {
            // continuation byte (0x80..=0xBF)
            if self.str_incomplete > 0 {
                self.str_incomplete += 1;
                if self.str_incomplete >= self.str_char_len {
                    self.str_incomplete = 0;
                }
            }
        }
    }

    fn cur_pos(&self) -> Pos {
        self.frames.last().map(|f| f.pos).unwrap_or(self.top_pos)
    }

    fn set_pos(&mut self, p: Pos) {
        match self.frames.last_mut() {
            Some(f) => f.pos = p,
            None => self.top_pos = p,
        }
    }

    fn cur_drop_to(&self) -> usize {
        self.frames
            .last()
            .map(|f| f.elem_drop_to)
            .unwrap_or(0)
    }

    fn value_allowed(&self) -> bool {
        matches!(
            self.cur_pos(),
            Pos::ArrBeforeElem | Pos::ObjBeforeVal | Pos::TopBefore
        )
    }

    fn process(&mut self, b: u8) {
        let off = self.len;
        self.len += 1;
        if !self.consistent {
            return;
        }
        match self.lex {
            Lex::Str => match b {
                b'\\' => {
                    self.lex = Lex::StrEsc;
                    self.str_incomplete = 0;
                }
                b'"' => {
                    self.lex = Lex::Between;
                    self.complete_string();
                }
                _ => self.track_utf8(b),
            },
            Lex::StrEsc => {
                self.lex = if b == b'u' { Lex::StrU(0) } else { Lex::Str };
            }
            Lex::StrU(n) => {
                if is_hex(b) {
                    self.lex = if n == 3 { Lex::Str } else { Lex::StrU(n + 1) };
                } else {
                    self.consistent = false;
                }
            }
            Lex::Scalar => {
                if is_scalar_byte(b) {
                    self.scalar_buf.push(b);
                } else {
                    self.lex = Lex::Between;
                    self.complete_scalar();
                    if self.consistent {
                        self.process_between(b, off);
                    }
                }
            }
            Lex::Between => self.process_between(b, off),
        }
    }

    fn complete_string(&mut self) {
        if self.str_is_key {
            self.set_pos(Pos::ObjAfterKey);
        } else {
            self.after_value();
        }
    }

    fn complete_scalar(&mut self) {
        if !is_valid_scalar(&self.scalar_buf) {
            self.consistent = false;
            return;
        }
        self.after_value();
    }

    /// Advance position after a complete value (string value, scalar, or
    /// closed container) was produced in the current value position.
    fn after_value(&mut self) {
        match self.cur_pos() {
            Pos::ArrBeforeElem => {
                self.set_pos(Pos::ArrAfterElem);
                if let Some(f) = self.frames.last_mut() {
                    f.seen_member = true;
                }
            }
            Pos::ObjBeforeVal => {
                self.set_pos(Pos::ObjAfterVal);
                if let Some(f) = self.frames.last_mut() {
                    f.seen_member = true;
                }
            }
            Pos::TopBefore => self.top_pos = Pos::TopAfter,
            _ => self.consistent = false,
        }
    }

    fn process_between(&mut self, b: u8, off: usize) {
        if is_ws(b) {
            return;
        }
        match b {
            b'"' => {
                match self.cur_pos() {
                    Pos::ObjBeforeKey => self.str_is_key = true,
                    Pos::ArrBeforeElem | Pos::ObjBeforeVal | Pos::TopBefore => {
                        self.str_is_key = false
                    }
                    _ => {
                        self.consistent = false;
                        return;
                    }
                }
                self.lex = Lex::Str;
                self.str_incomplete = 0;
                self.str_char_len = 0;
            }
            b'{' => {
                if !self.value_allowed() {
                    self.consistent = false;
                    return;
                }
                self.frames.push(FrameState {
                    frame: Frame::Object,
                    pos: Pos::ObjBeforeKey,
                    elem_drop_to: off + 1,
                    seen_member: false,
                });
            }
            b'[' => {
                if !self.value_allowed() {
                    self.consistent = false;
                    return;
                }
                self.frames.push(FrameState {
                    frame: Frame::Array,
                    pos: Pos::ArrBeforeElem,
                    elem_drop_to: off + 1,
                    seen_member: false,
                });
            }
            b'}' => self.close(Frame::Object),
            b']' => self.close(Frame::Array),
            b':' => {
                if self.cur_pos() == Pos::ObjAfterKey {
                    self.set_pos(Pos::ObjBeforeVal);
                } else {
                    self.consistent = false;
                }
            }
            b',' => match self.cur_pos() {
                Pos::ArrAfterElem => {
                    self.set_pos(Pos::ArrBeforeElem);
                    if let Some(f) = self.frames.last_mut() {
                        f.elem_drop_to = off;
                    }
                }
                Pos::ObjAfterVal => {
                    self.set_pos(Pos::ObjBeforeKey);
                    if let Some(f) = self.frames.last_mut() {
                        f.elem_drop_to = off;
                    }
                }
                _ => self.consistent = false,
            },
            _ => {
                if is_scalar_start(b) && self.value_allowed() {
                    self.lex = Lex::Scalar;
                    self.scalar_buf.clear();
                    self.scalar_buf.push(b);
                } else {
                    self.consistent = false;
                }
            }
        }
    }

    fn close(&mut self, want: Frame) {
        let ok = match self.frames.last() {
            Some(f) if f.frame == want => match (want, f.pos) {
                (Frame::Object, Pos::ObjAfterVal) => true,
                (Frame::Object, Pos::ObjBeforeKey) => !f.seen_member,
                (Frame::Array, Pos::ArrAfterElem) => true,
                (Frame::Array, Pos::ArrBeforeElem) => !f.seen_member,
                _ => false,
            },
            _ => false,
        };
        if !ok {
            self.consistent = false;
            return;
        }
        self.frames.pop();
        self.after_value();
    }

    fn cur_seen_member(&self) -> bool {
        self.frames.last().map(|f| f.seen_member).unwrap_or(false)
    }

    /// Append-only repair: keep already-emitted bytes, append only closers.
    /// Suited to streaming passthrough. See `AppendRepair`.
    pub fn append_repair(&self) -> AppendRepair {
        if !self.consistent {
            return AppendRepair { consistent: false, safe: false, append: Vec::new() };
        }
        let mut append: Vec<u8> = Vec::new();
        let safe = match self.lex {
            Lex::Str => {
                if self.str_is_key || self.str_incomplete > 0 {
                    false
                } else {
                    append.push(b'"');
                    true
                }
            }
            Lex::StrEsc | Lex::StrU(_) => false,
            Lex::Scalar => is_valid_scalar(&self.scalar_buf),
            Lex::Between => match self.cur_pos() {
                Pos::TopBefore => {
                    return AppendRepair { consistent: true, safe: true, append: Vec::new() };
                }
                Pos::TopAfter | Pos::ArrAfterElem | Pos::ObjAfterVal => true,
                Pos::ArrBeforeElem | Pos::ObjBeforeKey => !self.cur_seen_member(),
                Pos::ObjAfterKey | Pos::ObjBeforeVal => false,
            },
        };
        if !safe {
            return AppendRepair { consistent: true, safe: false, append: Vec::new() };
        }
        for f in self.frames.iter().rev() {
            append.push(match f.frame {
                Frame::Object => b'}',
                Frame::Array => b']',
            });
        }
        AppendRepair { consistent: true, safe: true, append }
    }

    pub fn finish(&self) -> Repair {
        if !self.consistent {
            return Repair {
                consistent: false,
                drop_trailing: 0,
                append: Vec::new(),
            };
        }
        let mut drop_trailing = 0usize;
        let mut append: Vec<u8> = Vec::new();

        // 1) Resolve the in-progress lexer token.
        match self.lex {
            Lex::Str => {
                if self.str_is_key {
                    drop_trailing = self.len - self.cur_drop_to();
                } else {
                    drop_trailing = self.str_incomplete;
                    append.push(b'"');
                }
            }
            Lex::StrEsc => {
                if self.str_is_key {
                    drop_trailing = self.len - self.cur_drop_to();
                } else {
                    drop_trailing = 1; // drop the dangling '\'
                    append.push(b'"');
                }
            }
            Lex::StrU(n) => {
                if self.str_is_key {
                    drop_trailing = self.len - self.cur_drop_to();
                } else {
                    drop_trailing = 2 + n as usize; // drop '\u' + n hex digits
                    append.push(b'"');
                }
            }
            Lex::Scalar => {
                let is_keyword = matches!(self.scalar_buf.as_slice(), b"true" | b"false" | b"null");
                if is_keyword {
                    // complete keyword: keep it; frames closed below
                } else if self.frames.is_empty() {
                    // top-level bare scalar: out of scope, leave unchanged
                } else {
                    drop_trailing = self.len - self.cur_drop_to();
                }
            }
            Lex::Between => match self.cur_pos() {
                Pos::TopAfter | Pos::ArrAfterElem | Pos::ObjAfterVal => {}
                Pos::TopBefore => {
                    return Repair {
                        consistent: true,
                        drop_trailing: 0,
                        append: Vec::new(),
                    };
                }
                Pos::ArrBeforeElem | Pos::ObjBeforeKey | Pos::ObjAfterKey | Pos::ObjBeforeVal => {
                    drop_trailing = self.len - self.cur_drop_to();
                }
            },
        }

        // 2) Close every open frame, innermost first.
        for f in self.frames.iter().rev() {
            append.push(match f.frame {
                Frame::Object => b'}',
                Frame::Array => b']',
            });
        }

        Repair {
            consistent: true,
            drop_trailing,
            append,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::repair_str;
    use serde_json::Value;
    use super::StreamRepairer;

    /// Apply an engine repair at the raw-byte level (for testing inputs that are
    /// not valid UTF-8, which `repair_str` cannot accept).
    fn engine_repair(bytes: &[u8]) -> Option<Vec<u8>> {
        let mut r = StreamRepairer::new();
        r.push(bytes);
        let rep = r.finish();
        if !rep.consistent {
            return None;
        }
        let keep = bytes.len() - rep.drop_trailing;
        let mut out = bytes[..keep].to_vec();
        out.extend_from_slice(&rep.append);
        Some(out)
    }

    #[test]
    fn truncated_multibyte_char_in_string_value_is_utf8_safe() {
        // `{"a":"x` followed by the lead byte 0xC3 of 'é' (continuation missing)
        let mut bytes = br#"{"a":"x"#.to_vec();
        bytes.push(0xC3);
        let out = engine_repair(&bytes).expect("should be consistent");
        let s = std::str::from_utf8(&out).expect("output must be valid UTF-8");
        serde_json::from_str::<Value>(s).expect("output must parse");
        assert_eq!(s, r#"{"a":"x"}"#);
    }

    #[test]
    fn truncated_emoji_in_string_value_is_utf8_safe() {
        // `["` + 3 of the 4 bytes of 😀 (F0 9F 98 80)
        let mut bytes = br#"["#.to_vec();
        bytes.push(b'"');
        bytes.extend_from_slice(&[0xF0, 0x9F, 0x98]);
        let out = engine_repair(&bytes).expect("should be consistent");
        let s = std::str::from_utf8(&out).expect("output must be valid UTF-8");
        serde_json::from_str::<Value>(s).expect("output must parse");
        assert_eq!(s, r#"[""]"#);
    }

    #[test]
    fn complete_multibyte_char_kept() {
        let out = engine_repair("{\"a\":\"café".as_bytes()).expect("consistent");
        let s = std::str::from_utf8(&out).unwrap();
        assert_eq!(s, r#"{"a":"café"}"#);
    }

    #[test]
    fn malformed_delimited_scalars_are_inconsistent() {
        assert_eq!(crate::repair_str(r#"{"a":truee}"#), None);
        assert_eq!(crate::repair_str("[truee]"), None);
        assert_eq!(crate::repair_str("[nulll]"), None);
        assert_eq!(crate::repair_str("[falsee]"), None);
        assert_eq!(crate::repair_str("[1e5e5]"), None);
        assert_eq!(crate::repair_str("[1..2]"), None);
        assert_eq!(crate::repair_str("[--1]"), None);
        assert_eq!(crate::repair_str("[1,2tru]"), None);
    }

    #[test]
    fn valid_numbers_still_accepted() {
        assert_repairs("[0,-0,1.5,-2e10,3.14,1E-5", "[0,-0,1.5,-2e10,3.14]");
    }

    /// Assert the repaired output parses as JSON.
    fn assert_repairs(input: &str, expected: &str) {
        let got = repair_str(input).expect("should be consistent");
        assert_eq!(got, expected, "input: {input:?}");
        serde_json::from_str::<Value>(&got).expect("repaired output must parse");
    }

    #[test]
    fn closes_truncated_string_value() {
        assert_repairs(
            r#"{"id":42,"generation":"The application sequence failed due to an error"#,
            r#"{"id":42,"generation":"The application sequence failed due to an error"}"#,
        );
    }

    #[test]
    fn empty_containers() {
        assert_repairs("{", "{}");
        assert_repairs("[", "[]");
        assert_repairs("{}", "{}");
        assert_repairs("[]", "[]");
    }

    #[test]
    fn nested_containers_closed_in_order() {
        assert_repairs(r#"{"a":["x",{"b":"c"#, r#"{"a":["x",{"b":"c"}]}"#);
    }

    #[test]
    fn drops_incomplete_object_key() {
        assert_repairs(r#"{"ab"#, "{}");
        assert_repairs(r#"{"a":"v","b"#, r#"{"a":"v"}"#);
    }

    #[test]
    fn drops_dangling_colon_value_position() {
        assert_repairs(r#"{"a":"#, "{}");
        assert_repairs(r#"{"x":"v","a":"#, r#"{"x":"v"}"#);
    }

    #[test]
    fn drops_incomplete_scalar_in_array() {
        assert_repairs(r#"{"status":"partial","payload_metrics":[250,194,"#,
                       r#"{"status":"partial","payload_metrics":[250,194]}"#);
        assert_repairs("[1,2,3", "[1,2]");
        assert_repairs("[1,2,", "[1,2]");
    }

    #[test]
    fn drops_incomplete_scalar_object_value() {
        assert_repairs(r#"{"a":1"#, "{}");
        assert_repairs(r#"{"x":1,"a":2"#, r#"{"x":1}"#);
    }

    #[test]
    fn keeps_complete_value_then_closes() {
        assert_repairs(r#"{"a":"b","c":"d"#, r#"{"a":"b","c":"d"}"#);
        assert_repairs(r#"[true,false,null"#, "[true,false,null]");
    }

    #[test]
    fn top_level_string_value_closed() {
        assert_repairs(r#""hello wor"#, r#""hello wor""#);
    }

    #[test]
    fn whitespace_tolerated() {
        assert_repairs("{  \"a\" : \"b\" , ", r#"{  "a" : "b" }"#);
    }

    #[test]
    fn escaped_quote_does_not_close_string() {
        assert_repairs(r#"{"a":"he said \"hi\" to me"#, r#"{"a":"he said \"hi\" to me"}"#);
    }

    #[test]
    fn escaped_backslash_then_quote_closes() {
        assert_repairs(r#"["c:\\path"#, r#"["c:\\path"]"#);
    }

    #[test]
    fn drops_dangling_backslash() {
        assert_repairs(r#"{"a":"line\"#, r#"{"a":"line"}"#);
    }

    #[test]
    fn drops_incomplete_unicode_escape() {
        assert_repairs(r#"{"a":"caf\u00"#, r#"{"a":"caf"}"#);
        assert_repairs(r#"{"a":"x\u"#, r#"{"a":"x"}"#);
    }

    #[test]
    fn complete_unicode_escape_kept() {
        assert_repairs(r#"{"a":"café and more"#, r#"{"a":"café and more"}"#);
    }

    #[test]
    fn mismatched_closer_is_inconsistent() {
        assert_eq!(crate::repair_str("[}"), None);
        assert_eq!(crate::repair_str("{]"), None);
    }

    #[test]
    fn underflow_closer_is_inconsistent() {
        assert_eq!(crate::repair_str("}"), None);
        assert_eq!(crate::repair_str("[1]]"), None);
    }

    #[test]
    fn trailing_comma_before_close_is_inconsistent() {
        assert_eq!(crate::repair_str("[1,]"), None);
        assert_eq!(crate::repair_str(r#"{"a":1,}"#), None);
    }

    #[test]
    fn missing_comma_between_values_is_inconsistent() {
        assert_eq!(crate::repair_str("[1 2]"), None);
    }

    #[test]
    fn second_top_level_value_is_inconsistent() {
        assert_eq!(crate::repair_str("{}{}"), None);
    }

    /// Apply an append-only repair to a string input.
    fn append_repair_str(input: &str) -> Option<String> {
        let mut r = StreamRepairer::new();
        r.push(input.as_bytes());
        let ar = r.append_repair();
        if !ar.consistent || !ar.safe {
            return None;
        }
        let mut out = input.as_bytes().to_vec();
        out.extend_from_slice(&ar.append);
        Some(String::from_utf8(out).unwrap())
    }

    #[test]
    fn append_closes_mid_string_value() {
        assert_eq!(append_repair_str(r#"{"a":"hello wor"#).as_deref(), Some(r#"{"a":"hello wor"}"#));
    }

    #[test]
    fn append_keeps_complete_scalar_value() {
        assert_eq!(append_repair_str(r#"{"count":123"#).as_deref(), Some(r#"{"count":123}"#));
        assert_eq!(append_repair_str("[1,2,3").as_deref(), Some("[1,2,3]"));
        assert_eq!(append_repair_str("[true,false").as_deref(), Some("[true,false]"));
    }

    #[test]
    fn append_closes_nested() {
        assert_eq!(append_repair_str(r#"{"a":["x",{"b":"c"#).as_deref(), Some(r#"{"a":["x",{"b":"c"}]}"#));
    }

    #[test]
    fn append_empty_containers() {
        assert_eq!(append_repair_str("{").as_deref(), Some("{}"));
        assert_eq!(append_repair_str("[").as_deref(), Some("[]"));
    }

    #[test]
    fn append_unsafe_cases_return_none() {
        assert_eq!(append_repair_str("[1,2,"), None);
        assert_eq!(append_repair_str(r#"{"a":1,"#), None);
        assert_eq!(append_repair_str(r#"{"a":1."#), None);
        assert_eq!(append_repair_str(r#"{"a":1e"#), None);
        assert_eq!(append_repair_str("[tru"), None);
        assert_eq!(append_repair_str(r#"{"a"#), None);
        assert_eq!(append_repair_str(r#"{"a":"#), None);
        assert_eq!(append_repair_str(r#"{"a":"x\"#), None);
        assert_eq!(append_repair_str(r#"{"a":"x\u00"#), None);
    }

    #[test]
    fn append_noop_on_complete_json() {
        let mut r = StreamRepairer::new();
        r.push(r#"{"a":[1,2]}"#.as_bytes());
        let ar = r.append_repair();
        assert!(ar.consistent && ar.safe && ar.is_noop());
    }

    #[test]
    fn append_inconsistent_propagates() {
        let mut r = StreamRepairer::new();
        r.push("[}".as_bytes());
        let ar = r.append_repair();
        assert!(!ar.consistent);
    }
}
