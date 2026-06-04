# Security Policy

## Reporting a vulnerability

Please report security issues privately via
[GitHub Security Advisories](https://github.com/tensorhq/suture-stream-repair/security/advisories/new)
rather than a public issue. We aim to acknowledge within a few business days.

## Security model

Suture is a reverse proxy that **forwards your provider credential verbatim and stores
nothing** — it holds no API keys. For AWS Bedrock it forwards a SigV4-signed request, so the
secret access key never reaches the proxy at all (only a per-request signature).

Areas where reports are especially valuable:

- **SSRF / upstream-host steering** — the proxy derives upstream hosts from request
  metadata. `is_bedrock_host` and `vertex_host` validate hosts to AWS/GCP; any way to steer
  the upstream elsewhere is a serious issue.
- **Stream corruption** — any input that makes the repairer emit invalid output for a target
  it reports as safely repaired.
- **Panics / DoS** — any input that panics the engine or causes unbounded buffering.

## Scope

In scope: the `suture-repair`, `suture-repair-sse`, and `suture-repair-core` crates.
Out of scope: vulnerabilities in upstream providers or in dependencies (report those upstream).
