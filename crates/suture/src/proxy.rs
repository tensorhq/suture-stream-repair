//! Reverse proxy router and handler.

use crate::config::Config;
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Method, StatusCode, Uri},
    response::Response,
    routing::post,
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
        .with_state(cfg)
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
    let body_bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return text_status(StatusCode::BAD_REQUEST, "invalid request body"),
    };

    let url = upstream_url(&cfg, provider, &uri);
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

    let mut builder = Response::builder().status(status.as_u16());
    for (k, v) in upstream.headers().iter() {
        if k == header::TRANSFER_ENCODING
            || k == header::CONTENT_LENGTH
            || k == header::CONNECTION
        {
            continue;
        }
        builder = builder.header(k.as_str(), v.as_bytes());
    }

    if ctype.starts_with("text/event-stream") {
        let extractor: Box<dyn DeltaExtractor> = match provider {
            Provider::OpenAi => Box::new(OpenAi),
            Provider::Anthropic => Box::new(Anthropic),
        };
        let repaired = repair_stream(upstream.bytes_stream(), extractor);
        builder
            .body(Body::from_stream(repaired))
            .unwrap_or_else(|_| text_status(StatusCode::INTERNAL_SERVER_ERROR, "body error"))
    } else if ctype.starts_with("application/json") {
        let raw = match upstream.bytes().await {
            Ok(b) => b,
            Err(e) => return text_status(StatusCode::BAD_GATEWAY, &format!("upstream read: {e}")),
        };
        let out: bytes::Bytes = std::str::from_utf8(&raw)
            .ok()
            .and_then(suture_core::repair_str)
            .map(bytes::Bytes::from)
            .unwrap_or(raw);
        builder
            .body(Body::from(out))
            .unwrap_or_else(|_| text_status(StatusCode::INTERNAL_SERVER_ERROR, "body error"))
    } else {
        builder
            .body(Body::from_stream(upstream.bytes_stream()))
            .unwrap_or_else(|_| text_status(StatusCode::INTERNAL_SERVER_ERROR, "body error"))
    }
}

fn text_status(code: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(code)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(msg.to_string()))
        .unwrap()
}
