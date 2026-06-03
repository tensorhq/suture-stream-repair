# Deploying Suture

Suture is a stateless HTTP reverse proxy. Your app keeps using the normal
OpenAI/Anthropic SDK — you just point its `base_url` at Suture instead of the
provider. Suture forwards upstream (passing your API key through untouched — it
holds no secrets) and repairs truncated JSON on the way back.

```python
from openai import OpenAI
client = OpenAI(base_url="http://localhost:8787/v1", api_key=os.environ["OPENAI_API_KEY"])
```

Routes: `POST /v1/chat/completions` → OpenAI, `POST /v1/messages` → Anthropic,
`GET /health` → `200 ok`. Config via env: `SUTURE_LISTEN`, `SUTURE_OPENAI_BASE`,
`SUTURE_ANTHROPIC_BASE`.

## Build the image

```bash
docker build -t suture:latest .
```

## Deployment options

| File | Target | Notes |
|------|--------|-------|
| `cloudrun.yaml` | GCP Cloud Run | `gcloud run services replace deploy/cloudrun.yaml --region REGION` |
| `ecs-fargate-taskdef.json` | AWS ECS/Fargate | Register, then run behind an ALB |
| `k8s-sidecar.yaml` | GKE/EKS | **Recommended** — co-located sidecar, lowest latency |

The **sidecar** pattern (`k8s-sidecar.yaml`) best matches Suture's low-latency
design: Suture runs in the same pod/task as the app and is reached over
`localhost`, so the only added hop is in-pod.

## Operational notes

- **Don't buffer the stream.** The whole value is streaming SSE repair. Any
  load balancer / ingress in front of Suture must stream, not buffer responses
  (e.g. nginx: `proxy_buffering off`). ALB and Cloud Run stream by default.
- **TLS at the edge.** Suture serves plain HTTP (it uses TLS only outbound, to
  providers). Terminate client TLS at the LB/ingress; for a sidecar, localhost
  HTTP is fine.
- **Health checks.** Use an external HTTP `GET /health` (ALB target group, Cloud
  Run startup probe, K8s readinessProbe). The distroless image has no shell, so a
  container-internal `CMD-SHELL` health check won't work — use the LB/kubelet HTTP
  probe instead.
- **Auth passthrough.** Suture forwards the `Authorization` / `x-api-key` header
  to the upstream, so it stores no credentials. This works for OpenAI/Anthropic
  *direct*.

## GCP Vertex AI

Enable with `SUTURE_VERTEX_ENABLED=1`. Point your Vertex SDK / HTTP client at Suture;
Suture derives the regional upstream host from the request path's `locations/{region}`
segment, so no region config is needed (optional `SUTURE_VERTEX_BASE` overrides it).

- **Auth:** unchanged — your client supplies the GCP OAuth bearer token (its SDK
  auto-refreshes it); Suture forwards `Authorization` verbatim and stores nothing.
- **Claude-on-Vertex** (`:streamRawPredict`) returns Anthropic SSE and is repaired with
  the Anthropic logic.
- **Gemini** (`:streamGenerateContent`) must be called with **`?alt=sse`** so Suture sees
  an SSE stream; it repairs truncated JSON-mode output (`responseMimeType:
  application/json`). Without `?alt=sse`, Vertex returns a JSON array, which falls back to
  whole-body JSON repair.

## Compression

Suture forwards the client's `Accept-Encoding` to the upstream, decodes the response
(gzip/brotli/deflate), repairs it, and re-encodes for the client per its `Accept-Encoding`
(honoring q-values) — so compression no longer prevents repair. An upstream coding Suture
can't decode is passed through verbatim without repair (never corrupted).

## AWS Bedrock

Enable with `SUTURE_BEDROCK_ENABLED=1`. Point your AWS SDK / signer at Suture, but **sign
the request for the real Bedrock host** (`bedrock-runtime.{region}.amazonaws.com`) — Suture
derives the upstream from the (validated) `Host` header and forwards the signed request
verbatim, so SigV4 validates at AWS. Targets `ConverseStream`
(`/model/{id}/converse-stream`); truncated `toolUse.input` is repaired.

- **Auth / security:** SigV4 transmits only a signature, never the AWS secret key, so Suture
  holds and sees no reusable AWS credential. Suture validates the `Host` against
  `bedrock-runtime.{region}.amazonaws.com` (no open proxy). `SUTURE_BEDROCK_BASE` overrides
  the host for test/private endpoints.

## Not supported yet

- **OpenAI-compatible translation** to other providers — Suture forwards each provider's
  native API; it does not translate request/response schemas.
