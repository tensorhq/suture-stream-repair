use suture_core::{AppendRepair, StreamRepairer};

/// Identifies which JSON-bearing field a target tracks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetKind {
    /// `choices[choice].delta.content` (OpenAI) — only tracked when JSON-looking.
    Content { choice: usize },
    /// `choices[choice].delta.tool_calls[tool].function.arguments` (OpenAI).
    ToolArgs { choice: usize, tool: usize },
    /// Anthropic `content_block` at `index` carrying `input_json_delta` or
    /// JSON-looking `text_delta`.
    Block { index: usize },
}

/// One reassembled JSON target and its running repair state.
pub struct TargetState {
    pub kind: TargetKind,
    repairer: StreamRepairer,
    first_byte: Option<u8>,
    json_like: bool,
    always_repair: bool,
}

impl TargetState {
    fn new(kind: TargetKind, always_repair: bool) -> Self {
        Self {
            kind,
            repairer: StreamRepairer::new(),
            first_byte: None,
            json_like: false,
            always_repair,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        if self.first_byte.is_none() {
            if let Some(&b) = bytes
                .iter()
                .find(|&&b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
            {
                self.first_byte = Some(b);
                self.json_like = matches!(b, b'{' | b'[');
            }
        }
        self.repairer.push(bytes);
    }

    /// Whether this target should be considered for repair.
    pub fn repairable(&self) -> bool {
        self.always_repair || self.json_like
    }

    pub fn repair(&self) -> AppendRepair {
        self.repairer.append_repair()
    }
}

/// Ordered collection of targets plus stream-level metadata for synthesis.
#[derive(Default)]
pub struct Targets {
    order: Vec<TargetKind>,
    states: Vec<TargetState>,
    pub id: Option<String>,
    pub model: Option<String>,
}

impl Targets {
    pub fn new() -> Self {
        Self::default()
    }

    /// Route bytes to the target with this kind, creating it if needed.
    pub fn feed(&mut self, kind: TargetKind, always_repair: bool, bytes: &[u8]) {
        let idx = match self.order.iter().position(|k| k == &kind) {
            Some(i) => i,
            None => {
                self.order.push(kind.clone());
                self.states.push(TargetState::new(kind, always_repair));
                self.states.len() - 1
            }
        };
        self.states[idx].feed(bytes);
    }

    pub fn iter(&self) -> impl Iterator<Item = &TargetState> {
        self.states.iter()
    }
}
