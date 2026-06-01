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
use suture_sse::{repair_stream, Anthropic, DeltaExtractor, OpenAi};

#[derive(Clone, Copy)]
enum Provider {
    OpenAi,
    Anthropic,
}

/// Build the proxy router.
pub fn app(cfg: Arc<Config>) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(openai))
        .route("/v1/messages", post(anthropic))
        .route("/health", get(health))
        .with_state(cfg)
}

async fn health() -> &'static str {
    "ok"
}

async fn openai(
    State(cfg): State<Arc<Config>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    proxy(cfg, Provider::OpenAi, method, uri, headers, body).await
}

async fn anthropic(
    State(cfg): State<Arc<Config>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    proxy(cfg, Provider::Anthropic, method, uri, headers, body).await
}

fn upstream_url(cfg: &Config, provider: Provider, uri: &Uri) -> String {
    let base = match provider {
        Provider::OpenAi => &cfg.openai_base,
        Provider::Anthropic => &cfg.anthropic_base,
    };
    let path_q = uri.path_and_query().map(|p| p.as_str()).unwrap_or(uri.path());
    format!("{base}{path_q}")
}

async fn proxy(
    cfg: Arc<Config>,
    provider: Provider,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    use futures_util::StreamExt;

    let body_bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return text_status(StatusCode::BAD_REQUEST, "invalid request body"),
    };

    let url = upstream_url(&cfg, provider, &uri);
    let client_enc = pick_encoding(
        headers
            .get(header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok()),
    );

    let client = reqwest::Client::new();
    let mut rb = client.request(method, &url).body(body_bytes.to_vec());
    for (k, v) in headers.iter() {
        // Forward everything except hop-by-hop/length/host. Accept-Encoding IS now
        // forwarded (we decode the response ourselves).
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
        // We manage framing + encoding headers for the new body.
        if k == header::TRANSFER_ENCODING
            || k == header::CONTENT_LENGTH
            || k == header::CONNECTION
            || k == header::CONTENT_ENCODING
        {
            continue;
        }
        builder = builder.header(k.as_str(), v.as_bytes());
    }

    // Unknown upstream coding: cannot decode → pass through verbatim (never corrupt).
    if upstream_enc == Encoding::Unknown {
        if let Some(ce) = &orig_ce {
            builder = builder.header(header::CONTENT_ENCODING.as_str(), ce.as_bytes());
        }
        return builder
            .body(Body::from_stream(upstream.bytes_stream()))
            .unwrap_or_else(|_| text_status(StatusCode::INTERNAL_SERVER_ERROR, "body error"));
    }

    if ctype.starts_with("text/event-stream") {
        let extractor: Box<dyn DeltaExtractor> = match provider {
            Provider::OpenAi => Box::new(OpenAi),
            Provider::Anthropic => Box::new(Anthropic),
        };
        let raw = upstream
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other));
        let decoded = decode_stream(raw, upstream_enc);
        let repaired = repair_stream(decoded, extractor);
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
        // Other content types: pass through verbatim (re-add original encoding).
        if let Some(ce) = &orig_ce {
            builder = builder.header(header::CONTENT_ENCODING.as_str(), ce.as_bytes());
        }
        builder
            .body(Body::from_stream(upstream.bytes_stream()))
            .unwrap_or_else(|_| text_status(StatusCode::INTERNAL_SERVER_ERROR, "body error"))
    }
}

/// Choose the downstream body encoding from the client's `Accept-Encoding`
/// (preference: br > gzip > deflate > identity). q-values are ignored.
fn pick_encoding(accept: Option<&str>) -> Encoding {
    let a = accept.unwrap_or("").to_ascii_lowercase();
    if a.split(',').any(|t| t.trim().starts_with("br")) {
        Encoding::Brotli
    } else if a.contains("gzip") {
        Encoding::Gzip
    } else if a.contains("deflate") {
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
