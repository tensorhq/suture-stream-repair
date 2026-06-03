//! SSE-aware JSON stream repair for OpenAI and Anthropic streaming responses.

mod anthropic;
mod eventstream;
mod extractor;
mod gemini;
mod openai;
mod repairer;
mod sse_parse;
pub mod stream;
mod target;

pub use anthropic::Anthropic;
pub use extractor::{DeltaExtractor, Repair};
pub use gemini::Gemini;
pub use openai::OpenAi;
pub use repairer::SseRepairer;
pub use sse_parse::SseParser;
pub use stream::repair_stream;
pub use target::{TargetKind, Targets};
pub use eventstream::{build_frame, parse_frame, Frame, FrameError};
