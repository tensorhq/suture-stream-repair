/// Result of computing how to make a (possibly truncated) JSON byte stream valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    /// True if the consumed input was structurally consistent (no mismatched
    /// closing delimiter). When false, callers MUST NOT apply the repair and
    /// should pass the original bytes through untouched.
    pub consistent: bool,
    /// Number of bytes to remove from the END of all consumed input before
    /// appending `append` (removes incomplete trailing tokens / dangling commas).
    pub drop_trailing: usize,
    /// Bytes to append (after dropping `drop_trailing`) to yield valid JSON.
    pub append: Vec<u8>,
}

impl Repair {
    /// True when no change is needed.
    pub fn is_noop(&self) -> bool {
        self.drop_trailing == 0 && self.append.is_empty()
    }
}

/// Incremental JSON structural repair engine. See module docs for scope.
pub struct StreamRepairer {
    len: usize,
}

impl Default for StreamRepairer {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamRepairer {
    pub fn new() -> Self {
        Self { len: 0 }
    }

    /// Feed bytes incrementally. May be called any number of times.
    pub fn push(&mut self, bytes: &[u8]) {
        self.len += bytes.len();
    }

    /// Compute the repair for everything pushed so far. Non-mutating.
    pub fn finish(&self) -> Repair {
        // Stub: replaced in Task 2.
        Repair { consistent: true, drop_trailing: 0, append: Vec::new() }
    }
}
