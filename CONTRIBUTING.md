# Contributing to Suture

Thanks for your interest — contributions are welcome.

## Getting started

```sh
git clone https://github.com/tensorhq/suture-stream-repair
cd suture-stream-repair
cargo test --workspace
```

The workspace has three crates:

- `crates/suture-core` (`suture-repair-core`) — the byte-level JSON repair engine. Pure,
  sync, no I/O.
- `crates/suture-sse` (`suture-repair-sse`) — SSE / eventstream transport + per-provider
  extractors.
- `crates/suture` (`suture-repair`) — the axum/reqwest reverse proxy (lib + `suture` binary).

## Before you open a PR

CI runs these three; please run them locally first:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- **Tests first.** This codebase is test-driven; new behavior should come with tests. The
  core engine has a `serde_json`/`proptest` invariant — if you touch the state machine, keep
  it green.
- **Keep commits focused** and use clear, imperative commit messages.
- **No new dependencies** without a reason; this is a low-latency proxy.

## Good first issues

Look for the `good first issue` label. Areas that are easy to pick up: additional provider
extractors, more test coverage (malformed-upstream, large-body, deflate/brotli end-to-end),
and docs.

## Reporting bugs

Open an issue with a minimal reproduction — ideally the truncated stream (or input bytes)
and the expected vs. actual repaired output.

## License

By contributing you agree your work is dual-licensed under MIT OR Apache-2.0, matching the
project.
