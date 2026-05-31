//! SSE-aware JSON stream repair for OpenAI and Anthropic streaming responses.

mod anthropic;
mod extractor;
mod openai;
mod repairer;
mod sse_parse;
mod target;

pub use anthropic::Anthropic;
pub use extractor::{DeltaExtractor, Repair};
pub use openai::OpenAi;
pub use repairer::SseRepairer;
pub use sse_parse::SseParser;
pub use target::{TargetKind, Targets};
