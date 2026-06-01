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

## Not supported yet

- **AWS Bedrock / GCP Vertex AI** as upstreams: Bedrock uses binary
  `application/vnd.amazon.eventstream` framing + SigV4 auth; Vertex Gemini uses
  its own schema + GCP OAuth. Suture currently speaks only OpenAI/Anthropic
  *direct* SSE with bearer/`x-api-key` passthrough. Support requires transport
  codecs + auth adapters (a planned Phase 4); the `suture-core` repair engine is
  reusable as-is.
- **Compressed upstreams:** Suture forces identity encoding to the upstream and
  passes through (without repairing) any response that arrives content-encoded.
  End-to-end compression with repair is part of the Phase 4 transport work.
