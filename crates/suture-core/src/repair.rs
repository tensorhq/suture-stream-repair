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
    top_drop_to: usize,
    lex: Lex,
    consistent: bool,
    len: usize,
    /// Whether the current/just-finished string sits in an object-key position.
    str_is_key: bool,
    /// Number of scalar bytes consumed for the current scalar token.
    scalar_len: usize,
    /// Expected total length if the scalar is a complete keyword (4 for true/null,
    /// 5 for false). 0 means it's a number and can never be considered "complete"
    /// at EOF.
    scalar_keyword_len: usize,
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
            top_drop_to: 0,
            lex: Lex::Between,
            consistent: true,
            len: 0,
            str_is_key: false,
            scalar_len: 0,
            scalar_keyword_len: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.process(b);
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
            .unwrap_or(self.top_drop_to)
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
                b'\\' => self.lex = Lex::StrEsc,
                b'"' => {
                    self.lex = Lex::Between;
                    self.complete_string();
                }
                _ => {}
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
                    self.scalar_len += 1;
                } else {
                    self.lex = Lex::Between;
                    self.complete_scalar();
                    self.process_between(b, off);
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
                    self.scalar_len = 1;
                    // Keywords have a fixed expected total length; numbers do not.
                    self.scalar_keyword_len = match b {
                        b't' => 4, // true
                        b'f' => 5, // false
                        b'n' => 4, // null
                        _ => 0,   // numeric: never "complete" at EOF
                    };
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
                // A scalar is "complete" at EOF only if it's a keyword whose
                // full length has been consumed (true=4, false=5, null=4).
                // Numbers are always considered incomplete (could be "3" → "30").
                let scalar_complete = self.scalar_keyword_len > 0
                    && self.scalar_len == self.scalar_keyword_len;
                if scalar_complete {
                    // Treat as if we just finished the scalar normally — just
                    // need to close the open frames.
                } else if self.frames.is_empty() {
                    // Top-level bare scalar: out of scope, leave unchanged.
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
}
