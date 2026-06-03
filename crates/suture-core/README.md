# suture-core

The byte-level JSON stream-repair engine behind
[Suture](https://github.com/tensorhq/suture-stream-repair).

Given any prefix of a valid JSON value, it computes the characters needed to close it — or
reports that the input is structurally inconsistent and should pass through untouched. No
allocation beyond nesting depth; microsecond-scale.

```rust
use suture_core::repair_str;

// truncated mid-string → close the string and the object
assert_eq!(repair_str(r#"{"city":"Par"#).as_deref(), Some(r#"{"city":"Par"}"#));
// dangling comma is dropped before closing
assert_eq!(repair_str("[1,2,").as_deref(), Some("[1,2]"));
// structurally inconsistent → None (caller passes the original through)
assert_eq!(repair_str("[}"), None);
```

Part of the Suture workspace. Dual-licensed under MIT OR Apache-2.0.
