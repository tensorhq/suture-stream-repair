//! SSE-aware JSON stream repair for OpenAI and Anthropic streaming responses.

mod sse_parse;

pub use sse_parse::SseParser;
