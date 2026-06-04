# Suture

[![CI](https://github.com/tensorhq/suture-stream-repair/actions/workflows/ci.yml/badge.svg)](https://github.com/tensorhq/suture-stream-repair/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/suture-repair.svg)](https://crates.io/crates/suture-repair)
[![docs.rs](https://img.shields.io/docsrs/suture-repair)](https://docs.rs/suture-repair)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Ultra-low-latency reverse proxy that repairs truncated and malformed JSON in LLM streaming responses, on the fly.**

When an upstream LLM stream is cut off — by `max_tokens`, a context-window limit, or a
dropped socket — the JSON it was emitting (a tool call's `arguments`, or structured-output
`content`) is left unterminated, and your application throws `JSONDecodeError` /
`serde_json` "EOF while parsing" errors. Suture sits between your app and the provider,
watches the stream, and emits exactly the missing characters to make the **reassembled**
JSON valid — without buffering the stream or adding meaningful latency.

### The problem, concretely

A tool-call stream truncated at `max_tokens` leaves your client reassembling invalid JSON:

```jsonc
// what the client reassembles from the delta events:
{"city": "Par      // ← unterminated → JSONDecodeError / serde_json: EOF while parsing a string
```

Suture closes it on the wire, so the client gets valid JSON instead — no SDK changes, no
retry, no regenerated tokens:

```jsonc
{"city": "Par"}    // ← valid; the string and object are safely closed
```

### Symptoms this fixes

You're in the right place if your LLM app has thrown any of these on a streaming response:

- `json.decoder.JSONDecodeError: Unterminated string starting at: line 1 column …`
- `json.decoder.JSONDecodeError: Expecting value: line 1 column … (char …)`
- `serde_json::Error: EOF while parsing a string` / `EOF while parsing an object`
- `pydantic_core.ValidationError` on a truncated tool-call `arguments`
- Tool / function-call arguments that won't parse when the model hits `max_tokens`
- Truncated structured-output / JSON-mode `content` across streamed deltas

…on OpenAI, Anthropic, Google Vertex AI (Gemini / Claude), or AWS Bedrock.

## Features

- Repairs **OpenAI** (`/v1/chat/completions`), **Anthropic** (`/v1/messages`),
  **GCP Vertex AI** (Gemini + Claude-on-Vertex), and **AWS Bedrock** (`ConverseStream`)
  streaming responses.
- **SSE-aware** — repairs the *reassembled* tool-call arguments / structured content
  accumulated across delta events, not just raw wire bytes.
- **Streaming + compressed** — transparently decodes gzip/brotli/deflate, repairs, and
  re-encodes per the client's `Accept-Encoding`; never buffers the whole body. Added
  overhead is ~10 µs per chunk.
- **Holds no credentials** — your provider API key / bearer token is forwarded verbatim.
- The byte-level repair engine is usable as a standalone library:
  `cargo add suture-repair` (then `use suture::…`), or just the engine via
  `cargo add suture-repair-core` (`use suture_core::repair_str`).

## Quickstart

```sh
cargo install suture-repair    # installs the `suture` binary; or: docker build -t suture .
suture                         # listens on 127.0.0.1:8787
```

Point your SDK's base URL at Suture (your API key still flows through):

```python
from openai import OpenAI
client = OpenAI(base_url="http://localhost:8787/v1", api_key=os.environ["OPENAI_API_KEY"])
```

Routes: `POST /v1/chat/completions` → OpenAI, `POST /v1/messages` → Anthropic,
`POST /v1/projects/*` → Vertex, `POST /model/*` → Bedrock (each when enabled), `GET /health`.

## How it works

Three layers, each independently tested:

- **`suture-core`** — a byte-level JSON repair state machine. Given any prefix of a valid
  JSON value, it computes the characters needed to close it (or reports that the input is
  inconsistent and should pass through untouched). No allocation beyond nesting depth.
- **`suture-sse`** — an incremental SSE parser + per-provider extractors that reassemble
  the JSON-bearing field across delta events, drive the core engine, and synthesize a
  closing event at stream end (before the terminator).
- **`suture`** — an axum/reqwest reverse proxy. Forwards your request verbatim, then on
  the response: `text/event-stream` is repaired via the SSE layer; a single
  `application/json` body is closed with the core engine; anything else streams through
  unchanged.

## Configuration

| Env var | Default | Purpose |
|---|---|---|
| `SUTURE_LISTEN` | `127.0.0.1:8787` | listen address |
| `SUTURE_OPENAI_BASE` | `https://api.openai.com` | OpenAI upstream |
| `SUTURE_ANTHROPIC_BASE` | `https://api.anthropic.com` | Anthropic upstream |
| `SUTURE_VERTEX_ENABLED` | `0` | enable the Vertex route (host derived from the path) |
| `SUTURE_VERTEX_BASE` | — | optional Vertex upstream override |
| `SUTURE_BEDROCK_ENABLED` | `0` | enable the Bedrock route (host from the validated `Host` header) |
| `SUTURE_BEDROCK_BASE` | — | optional Bedrock upstream override |

## Deployment

See [`deploy/`](deploy/) for a `Dockerfile` and Cloud Run, ECS/Fargate, and
Kubernetes-sidecar manifests, plus operational notes (don't buffer the stream, TLS at the
edge, health checks). The sidecar pattern (co-located, localhost) best matches the
low-latency design.

## Status

OpenAI, Anthropic, GCP Vertex AI, and **AWS Bedrock** (`ConverseStream`) are supported, with
transparent compression handling. Bedrock uses credential-free **SigV4 passthrough** — the
client signs for the real Bedrock host and Suture forwards verbatim, so Suture never sees a
reusable AWS secret (the secret key never leaves the client; only a per-request signature
transits).

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option. Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in the work by you shall be dual-licensed as above, without any additional terms.
