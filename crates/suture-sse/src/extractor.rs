use crate::target::Targets;

/// A provider-specific adapter that interprets SSE event payloads, routes JSON
/// delta fragments into `Targets`, recognises the stream terminator, and
/// synthesizes the closing SSE bytes from computed repairs.
pub trait DeltaExtractor: Send {
    /// Interpret one SSE `data` payload, updating `targets`.
    fn on_event(&self, data: &[u8], targets: &mut Targets);

    /// True if this payload is the provider's stream terminator.
    fn is_terminator(&self, data: &[u8]) -> bool;

    /// Build the synthetic SSE bytes that close `repairs` (already filtered to
    /// safe, non-noop targets). `terminated` indicates the upstream already sent
    /// its terminator (so we don't duplicate it).
    fn synthesize(&self, repairs: &[Repair], targets: &Targets, terminated: bool) -> Vec<u8>;
}

/// A single resolved repair: which target, and the bytes to append to it.
pub struct Repair {
    pub kind: crate::target::TargetKind,
    pub append: Vec<u8>,
}

/// Escape raw bytes for embedding inside a JSON string literal. The repair tail
/// is always ASCII structural chars (`"`, `}`, `]`).
pub fn json_escape(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() + 2);
    for &b in bytes {
        match b {
            b'"' => s.push_str("\\\""),
            b'\\' => s.push_str("\\\\"),
            _ => s.push(b as char),
        }
    }
    s
}
