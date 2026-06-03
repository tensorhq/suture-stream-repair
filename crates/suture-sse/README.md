# suture-sse

The streaming-transport layer of [Suture](https://github.com/tensorhq/suture-stream-repair).

Incremental SSE parsing, per-provider delta extractors (OpenAI, Anthropic, Gemini), an AWS
`eventstream` binary frame codec (Bedrock `ConverseStream`), and `Stream` adapters that
drive the [`suture-repair-core`](https://crates.io/crates/suture-repair-core) repair engine
over the *reassembled* tool-call arguments / structured content carried across delta events.

Used by the [`suture-repair`](https://crates.io/crates/suture-repair) proxy; usable directly to repair a
provider's streaming response. Dual-licensed under MIT OR Apache-2.0.
