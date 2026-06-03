# Why your LLM tool calls silently break — and a ~10µs fix

If you stream tool calls or structured output from an LLM, you have almost certainly seen
one of these in production:

```
json.decoder.JSONDecodeError: Unterminated string starting at: line 1 column 12 (char 11)
serde_json::Error: EOF while parsing a string at line 1 column 4096
```

It usually shows up under load, on your longest and most important responses, and it's
maddening because *the model did its job* — it just got cut off. This post is about why
that happens, why the obvious fixes don't really work, and a small proxy
([Suture](https://github.com/tensorhq/suture-stream-repair)) that fixes it on the wire in
microseconds without touching your code or your API keys.

## What actually breaks

When you stream a chat completion, the provider doesn't send you one JSON document. It
sends a long sequence of Server-Sent Events, each a *complete, valid* little JSON object
carrying a fragment:

```
data: {"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"{\"ci"}}]}}]}
data: {"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"ty\":\"Par"}}]}}]}
data: {"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"is\"}"}}]}}]}
data: [DONE]
```

Your SDK **reassembles** the `arguments` field across all those events into one string —
`{"city":"Paris"}` — and *then* parses it. The catch: the thing that's actually JSON (the
tool arguments, or your structured-output `content`) lives *inside* those fragments and is
only complete once the whole stream arrives.

So when the stream ends early — the model hits `max_tokens`, blows the context window, or
the socket just dies — you're left holding this:

```
{"city":"Par
```

The SSE envelope was fine. The reassembled JSON is not. Your parser throws.

## Why the obvious fixes don't work

- **Retry the request.** You pay for the whole long generation again, and it may truncate
  again the same way. Expensive and non-deterministic.
- **`try/except` and move on.** You throw away a response the model spent real tokens
  producing — often you can see the answer right there, just missing a `"}`.
- **Bigger `max_tokens`.** Pushes the cliff back; doesn't remove it. Socket deaths don't
  care about your token budget.
- **Hand-rolled "close the braces" logic in your app.** This is the right *idea*, and it's
  also where people quietly ship bugs — see the next section.

## "Just close the braces" is harder than it looks

The naive repair is "append the missing `]` and `}`." Consider a tool-args stream truncated
right after a comma:

```
{"items":[250,194,
```

The tempting fix is to append `]}` → `{"items":[250,194,]}`. That is **invalid JSON** — a
trailing comma. A correct repair has to *drop* the dangling comma first, then close:
`{"items":[250,194]}`. The same trap hides in partial numbers (`1.`, `1e`), partial
keywords (`tru`), incomplete `\uXXXX` escapes, and — the nastiest — a multibyte UTF-8
character sliced in half by the truncation, where naively appending `"` produces invalid
UTF-8 and a *different* crash.

Getting this right means treating it as what it is: a tiny, careful JSON parser. Suture's
core is a byte-level state machine with one invariant, checked by a property test against
`serde_json`: *for any prefix of any valid JSON value, the repaired output parses.* That
test caught the trailing-comma bug, the partial-scalar bugs, and a UTF-8-splitting panic
before any of them could ship.

## The approach: repair on the wire, see nothing you shouldn't

Suture is a reverse proxy. You point your SDK's `base_url` at it and change nothing else:

```python
client = OpenAI(base_url="http://localhost:8787/v1", api_key=os.environ["OPENAI_API_KEY"])
```

It forwards your request verbatim (your key just passes through — Suture stores nothing),
watches the streaming response, tracks the reassembled tool-args / structured content with
the byte-level engine, and at end-of-stream emits exactly the characters needed to close
it — as a final, well-formed delta event before the terminator. Your client reassembles
valid JSON and never knows anything was wrong.

Design choices that matter:

- **It's append-only and passthrough.** Complete events stream straight through untouched;
  Suture only appends a closing delta at the end. Added latency is ~10µs of CPU per chunk
  (measured with `criterion`) — three orders of magnitude under the time you spend waiting
  on the model.
- **It's content-aware, not byte-naive.** It repairs the *reassembled* field, and only
  JSON-bearing fields (tool arguments always; `content` only when it's actually JSON), so
  it never mangles prose.
- **It handles compression and four providers.** gzip/brotli/deflate are decoded, repaired,
  and re-encoded on the fly; OpenAI, Anthropic, Google Vertex (Gemini + Claude-on-Vertex),
  and AWS Bedrock (`ConverseStream`, a binary CRC-checked frame protocol) are all supported.

## A note on keys, because it should be a note

Suture forwards your credential and holds nothing. For **AWS Bedrock** it's even stronger:
SigV4 signing means the secret access key never crosses the wire at all — only a
per-request signature — so a compromised proxy can't steal a reusable AWS credential. (We
validate the upstream `Host` to AWS, too; an SSRF that tried to exploit the `Host` header
was caught and fixed in review.)

## Honest limits

This isn't magic and it isn't for everything. Providers are shipping native
structured-output guarantees (strict schemas, constrained decoding) that reduce *malformed*
JSON — good. What they don't fix is **truncation**: a stream cut at the token cap or a dead
socket still leaves you with valid-but-incomplete JSON, across the long tail of models,
Bedrock, and older APIs. That residual is exactly what Suture is for. It also won't
resurrect data that never arrived — it makes what *did* arrive parseable.

## Try it

Suture is Rust, dual-licensed MIT/Apache-2.0, ~100 tests, on GitHub:
**<https://github.com/tensorhq/suture-stream-repair>**. The repair engine is a standalone
library if you'd rather repair in-process and keep even the response bytes off the network.

If your structured-output pipeline has ever thrown on a truncated stream, it's a one-line
`base_url` change to find out whether this helps.
