//! Suture: ultra-low latency streaming JSON repair proxy.
//!
//! Library surface re-exports the repair engine; the `suture` binary is the proxy.

pub mod config;
pub mod proxy;

pub use config::Config;

// Re-export the engine so `cargo add suture` gives the full library.
pub use suture_core::{repair_str, AppendRepair, Repair, StreamRepairer};
pub use suture_sse::{repair_stream, Anthropic, DeltaExtractor, OpenAi, SseRepairer};
