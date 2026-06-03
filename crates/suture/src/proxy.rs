//! Reverse proxy router and handler.

use crate::config::Config;
use crate::encoding::{decode_stream, encode_stream, Encoding};
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Method, StatusCode, Uri},
    response::Response,
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use suture_sse::{
    eventstream_repair_stream, repair_stream, Anthropic, DeltaExtractor, Gemini, OpenAi,
};

/// What kind of repair to apply to the upstream response.
enum RepairKind {
    Sse(Box<dyn DeltaExtractor>),
    EventStreamConverse,
}

/// Build the proxy router.
pub fn app(cfg: Arc<Config>) -> Router {
    let mut router = Router::new()
        .route("/v1/chat/completions", post(openai))
        .route("/v1/messages", post(anthropic))
        .route("/health", get(health));
    if cfg.vertex_enabled {
        router = router.route("/v1/projects/*rest", post(vertex));
    }
    if cfg.bedrock_enabled {
        router = router.route("/model/*rest", post(bedrock));
    }
    router.with_state(cfg)
}

async fn health() -> &'static str {
    "ok"
}

fn path_and_query(uri: &Uri) -> &str {
    uri.path_and_query().map(|p| p.as_str()).unwrap_or(uri.path())
}

async fn openai(
    State(cfg): State<Arc<Config>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let url = format!("{}{}", cfg.openai_base, path_and_query(&uri));
    proxy(url, RepairKind::Sse(Box::new(OpenAi)), method, headers, body).await
}

async fn anthropic(
    State(cfg): State<Arc<Config>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let url = format!("{}{}", cfg.anthropic_base, path_and_query(&uri));
    proxy(url, RepairKind::Sse(Box::new(Anthropic)), method, headers, body).await
}

async fn vertex(
    State(cfg): State<Arc<Config>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let host = match vertex_host(&cfg, uri.path()) {
        Some(h) => h,
        None => return text_status(StatusCode::BAD_REQUEST, "vertex: cannot derive region from path"),
    };
    let url = format!("{host}{}", path_and_query(&uri));
    let extractor = vertex_extractor(uri.path());
    proxy(url, RepairKind::Sse(extractor), method, headers, body).await
}

/// Derive the Vertex upstream host from the request path's `locations/{region}`
/// segment (or use `SUTURE_VERTEX_BASE` if set). Returns None if neither works.
fn vertex_host(cfg: &Config, path: &str) -> Option<String> {
    if let Some(base) = &cfg.vertex_base {
        return Some(base.clone());
    }
    let region = path.split('/').skip_while(|s| *s != "locations").nth(1)?;
    if region.is_empty() {
        return None;
    }
    if region == "global" {
        Some("https://aiplatform.googleapis.com".to_string())
    } else {
        Some(format!("https://{region}-aiplatform.googleapis.com"))
    }
}

/// Select the repair extractor for a Vertex request by path. `streamGenerateContent`
/// / `publishers/google` is Gemini; everything else (notably `streamRawPredict` /
/// `publishers/anthropic`) uses the Anthropic extractor, which safely no-ops on
/// non-Anthropic events.
fn vertex_extractor(path: &str) -> Box<dyn DeltaExtractor> {
    if path.contains(":streamGenerateContent") || path.contains("/publishers/google/") {
        Box::new(Gemini)
    } else {
        Box::new(Anthropic)
    }
}

async fn bedrock(
    State(cfg): State<Arc<Config>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let base = match bedrock_host(&cfg, &headers) {
        Some(b) => b,
        None => return text_status(StatusCode::FORBIDDEN, "bedrock: missing or non-AWS Host header"),
    };
    let url = format!("{base}{}", path_and_query(&uri));
    proxy(url, RepairKind::EventStreamConverse, method, headers, body).await
}

/// Resolve the Bedrock upstream base: the `SUTURE_BEDROCK_BASE` override, else the
/// request's `Host` header if it is a valid Bedrock runtime host (so SigV4 stays valid).
fn bedrock_host(cfg: &Config, headers: &HeaderMap) -> Option<String> {
    if let Some(base) = &cfg.bedrock_base {
        return Some(base.clone());
    }
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok())?;
    if is_bedrock_host(host) {
        Some(format!("https://{host}"))
    } else {
        None
    }
}

/// True if `host` is `bedrock-runtime.{region}.amazonaws.com` (single-label region),
/// optionally with a port. Anchors the upstream to AWS — no open proxy.
fn is_bedrock_host(host: &str) -> bool {
    let h = host.split(':').next().unwrap_or(host);
    let Some(rest) = h.strip_prefix("bedrock-runtime.") else {
        return false;
    };
    let Some(region) = rest.strip_suffix(".amazonaws.com") else {
        return false;
    };
    !region.is_empty() && !region.contains('.') && region.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

async fn proxy(
    url: String,
    repair: RepairKind,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    use futures_util::StreamExt;

    let body_bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return text_status(StatusCode::BAD_REQUEST, "invalid request body"),
    };

    let client_enc = pick_encoding(
        headers
            .get(header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok()),
    );

    let client = reqwest::Client::new();
    let mut rb = client.request(method, &url).body(body_bytes.to_vec());
    for (k, v) in headers.iter() {
        if k == header::HOST || k == header::CONTENT_LENGTH || k == header::CONNECTION {
            continue;
        }
        rb = rb.header(k.as_str(), v.as_bytes());
    }

    let upstream = match rb.send().await {
        Ok(r) => r,
        Err(e) => return text_status(StatusCode::BAD_GATEWAY, &format!("upstream error: {e}")),
    };

    let status = upstream.status();
    let ctype = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let orig_ce = upstream.headers().get(header::CONTENT_ENCODING).cloned();
    let upstream_enc = orig_ce
        .as_ref()
        .and_then(|v| v.to_str().ok())
        .map(Encoding::from_token)
        .unwrap_or(Encoding::Identity);

    let mut builder = Response::builder().status(status.as_u16());
    for (k, v) in upstream.headers().iter() {
        if k == header::TRANSFER_ENCODING
            || k == header::CONTENT_LENGTH
            || k == header::CONNECTION
            || k == header::CONTENT_ENCODING
        {
            continue;
        }
        builder = builder.header(k.as_str(), v.as_bytes());
    }

    let (sse_extractor, want_eventstream) = match repair {
        RepairKind::Sse(e) => (Some(e), false),
        RepairKind::EventStreamConverse => (None, true),
    };

    if upstream_enc == Encoding::Unknown {
        if let Some(ce) = &orig_ce {
            builder = builder.header(header::CONTENT_ENCODING.as_str(), ce.as_bytes());
        }
        return builder
            .body(Body::from_stream(upstream.bytes_stream()))
            .unwrap_or_else(|_| text_status(StatusCode::INTERNAL_SERVER_ERROR, "body error"));
    }

    if ctype.starts_with("text/event-stream") {
        if let Some(extractor) = sse_extractor {
            let raw = upstream.bytes_stream().map(|r| r.map_err(std::io::Error::other));
            let decoded = decode_stream(raw, upstream_enc);
            let repaired = repair_stream(decoded, extractor);
            let out = encode_stream(repaired, client_enc);
            if let Some(ce) = client_enc.header_value() {
                builder = builder.header(header::CONTENT_ENCODING.as_str(), ce);
            }
            builder
                .body(Body::from_stream(out))
                .unwrap_or_else(|_| text_status(StatusCode::INTERNAL_SERVER_ERROR, "body error"))
        } else {
            builder
                .body(Body::from_stream(upstream.bytes_stream()))
                .unwrap_or_else(|_| text_status(StatusCode::INTERNAL_SERVER_ERROR, "body error"))
        }
    } else if ctype.starts_with("application/vnd.amazon.eventstream") && want_eventstream {
        let raw = upstream.bytes_stream().map(|r| r.map_err(std::io::Error::other));
        let decoded = decode_stream(raw, upstream_enc);
        let repaired = eventstream_repair_stream(decoded);
        let out = encode_stream(repaired, client_enc);
        if let Some(ce) = client_enc.header_value() {
            builder = builder.header(header::CONTENT_ENCODING.as_str(), ce);
        }
        builder
            .body(Body::from_stream(out))
            .unwrap_or_else(|_| text_status(StatusCode::INTERNAL_SERVER_ERROR, "body error"))
    } else if ctype.starts_with("application/json") {
        let raw = upstream
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other));
        let decoded = decode_stream(raw, upstream_enc);
        match collect_io(decoded).await {
            Ok(buf) => {
                let out: bytes::Bytes = std::str::from_utf8(&buf)
                    .ok()
                    .and_then(suture_core::repair_str)
                    .map(bytes::Bytes::from)
                    .unwrap_or_else(|| bytes::Bytes::from(buf));
                builder
                    .body(Body::from(out))
                    .unwrap_or_else(|_| text_status(StatusCode::INTERNAL_SERVER_ERROR, "body error"))
            }
            Err(e) => text_status(StatusCode::BAD_GATEWAY, &format!("decode error: {e}")),
        }
    } else {
        if let Some(ce) = &orig_ce {
            builder = builder.header(header::CONTENT_ENCODING.as_str(), ce.as_bytes());
        }
        builder
            .body(Body::from_stream(upstream.bytes_stream()))
            .unwrap_or_else(|_| text_status(StatusCode::INTERNAL_SERVER_ERROR, "body error"))
    }
}

/// Choose the downstream body encoding from the client's `Accept-Encoding`.
/// Honors q-values: a coding with `q=0` is "not acceptable" and is dropped.
/// Among acceptable codings we support, preference is br > gzip > deflate; otherwise
/// identity (always acceptable).
fn pick_encoding(accept: Option<&str>) -> Encoding {
    let accept = match accept {
        Some(a) => a,
        None => return Encoding::Identity,
    };
    let mut br = false;
    let mut gzip = false;
    let mut deflate = false;
    for part in accept.split(',') {
        let mut fields = part.split(';');
        let token = fields.next().unwrap_or("").trim().to_ascii_lowercase();
        if token.is_empty() {
            continue;
        }
        // Default q is 1.0; an explicit q=0 makes the coding unacceptable.
        let mut q = 1.0f32;
        for field in fields {
            let field = field.trim();
            if let Some(rest) = field.strip_prefix("q=") {
                q = rest.trim().parse().unwrap_or(0.0);
            }
        }
        if q <= 0.0 {
            continue;
        }
        match token.as_str() {
            "br" => br = true,
            "gzip" | "x-gzip" => gzip = true,
            "deflate" => deflate = true,
            _ => {}
        }
    }
    if br {
        Encoding::Brotli
    } else if gzip {
        Encoding::Gzip
    } else if deflate {
        Encoding::Deflate
    } else {
        Encoding::Identity
    }
}

async fn collect_io(mut s: crate::encoding::ByteStream) -> std::io::Result<Vec<u8>> {
    use futures_util::StreamExt;
    let mut out = Vec::new();
    while let Some(item) = s.next().await {
        out.extend_from_slice(&item?);
    }
    Ok(out)
}

fn text_status(code: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(code)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(msg.to_string()))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_encoding_honors_qvalues() {
        assert_eq!(pick_encoding(None), Encoding::Identity);
        assert_eq!(pick_encoding(Some("")), Encoding::Identity);
        assert_eq!(pick_encoding(Some("identity")), Encoding::Identity);
        assert_eq!(pick_encoding(Some("gzip")), Encoding::Gzip);
        assert_eq!(pick_encoding(Some("deflate")), Encoding::Deflate);
        // preference: br beats gzip when both acceptable
        assert_eq!(pick_encoding(Some("gzip, br")), Encoding::Brotli);
        // q=0 means NOT acceptable -> must be dropped
        assert_eq!(pick_encoding(Some("br;q=0")), Encoding::Identity);
        assert_eq!(pick_encoding(Some("gzip, br;q=0")), Encoding::Gzip);
        assert_eq!(pick_encoding(Some("gzip;q=0, identity")), Encoding::Identity);
        // whitespace / q with decimals
        assert_eq!(pick_encoding(Some("br; q=0.0, gzip; q=0.9")), Encoding::Gzip);
        assert_eq!(pick_encoding(Some(" gzip ")), Encoding::Gzip);
    }

    #[test]
    fn vertex_host_derives_region() {
        let cfg = Config::from_map(|_| None);
        assert_eq!(
            vertex_host(&cfg, "/v1/projects/p/locations/us-central1/publishers/google/models/gemini:streamGenerateContent").as_deref(),
            Some("https://us-central1-aiplatform.googleapis.com")
        );
        assert_eq!(
            vertex_host(&cfg, "/v1/projects/p/locations/global/publishers/anthropic/models/claude:streamRawPredict").as_deref(),
            Some("https://aiplatform.googleapis.com")
        );
        assert_eq!(vertex_host(&cfg, "/v1/projects/p/no-region-here").as_deref(), None);
    }

    #[test]
    fn vertex_host_override_wins() {
        let cfg = Config::from_map(|k| match k {
            "SUTURE_VERTEX_BASE" => Some("http://localhost:9/".to_string()),
            _ => None,
        });
        assert_eq!(
            vertex_host(&cfg, "/v1/projects/p/locations/eu/publishers/google/models/g:streamGenerateContent").as_deref(),
            Some("http://localhost:9")
        );
    }

    #[test]
    fn vertex_extractor_selection() {
        use suture_sse::{Repair, TargetKind, Targets};
        let targets = Targets::new();

        let gem = vertex_extractor("/v1/projects/p/locations/us/publishers/google/models/g:streamGenerateContent");
        let g_repairs = vec![Repair { kind: TargetKind::Content { choice: 0 }, append: b"\"}".to_vec() }];
        let g_out = String::from_utf8(gem.synthesize(&g_repairs, &targets, false)).unwrap();
        assert!(g_out.contains("candidates"), "google path -> Gemini: {g_out}");

        let ant = vertex_extractor("/v1/projects/p/locations/us/publishers/anthropic/models/c:streamRawPredict");
        let a_repairs = vec![Repair { kind: TargetKind::Block { index: 0 }, append: b"}".to_vec() }];
        let a_out = String::from_utf8(ant.synthesize(&a_repairs, &targets, false)).unwrap();
        assert!(a_out.contains("content_block_delta"), "anthropic path -> Anthropic: {a_out}");
    }

    #[test]
    fn bedrock_host_validates_aws_only() {
        assert!(is_bedrock_host("bedrock-runtime.us-east-1.amazonaws.com"));
        assert!(is_bedrock_host("bedrock-runtime.eu-west-1.amazonaws.com:443"));
        assert!(!is_bedrock_host("evil.com"));
        assert!(!is_bedrock_host("bedrock-runtime.evil.com.amazonaws.com"));
        assert!(!is_bedrock_host("bedrock-runtime.us-east-1.amazonaws.com.evil.com"));
        assert!(!is_bedrock_host("api.openai.com"));
        assert!(!is_bedrock_host("bedrock-runtime..amazonaws.com"));
    }

    #[test]
    fn bedrock_host_prefers_override_then_header() {
        use axum::http::HeaderMap;
        let cfg = Config::from_map(|k| match k {
            "SUTURE_BEDROCK_BASE" => Some("http://localhost:7".to_string()),
            _ => None,
        });
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "bedrock-runtime.us-east-1.amazonaws.com".parse().unwrap());
        assert_eq!(bedrock_host(&cfg, &h).as_deref(), Some("http://localhost:7"));

        let cfg2 = Config::from_map(|_| None);
        assert_eq!(bedrock_host(&cfg2, &h).as_deref(), Some("https://bedrock-runtime.us-east-1.amazonaws.com"));
        let bad = HeaderMap::new();
        assert_eq!(bedrock_host(&cfg2, &bad), None);
    }
}
