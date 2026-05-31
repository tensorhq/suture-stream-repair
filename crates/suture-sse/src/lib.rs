//! SSE-aware JSON stream repair for OpenAI and Anthropic streaming responses.

mod extractor;
mod openai;
mod sse_parse;
mod target;

pub use extractor::{DeltaExtractor, Repair};
pub use openai::OpenAi;
pub use sse_parse::SseParser;
pub use target::{TargetKind, Targets};
