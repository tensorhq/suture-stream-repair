use std::sync::Arc;

use axum::{body::Body, http::{header, HeaderMap}, response::Response, routing::post, Router};
use flate2::write::GzEncoder;
use flate2::Compression;
use std::io::Write;
use suture::{config::Config, proxy};

/// Spawn a server on an ephemeral port; return its base URL `http://127.0.0.1:PORT`.
async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn mock_openai_truncated() -> Response {
    let sse = "data: {\"id\":\"c1\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\\\"Par\"}}]}}]}\n\n";
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(sse))
        .unwrap()
}

async fn mock_json_truncated() -> Response {
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"id":42,"text":"hello"#))
        .unwrap()
}

#[tokio::test]
async fn proxies_and_repairs_truncated_sse() {
    let up = spawn(Router::new().route("/v1/chat/completions", post(mock_openai_truncated))).await;
    let cfg = Arc::new(Config::from_map(|k| match k {
        "SUTURE_OPENAI_BASE" => Some(up.clone()),
        _ => None,
    }));
    let proxy_url = spawn(proxy::app(cfg)).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("authorization", "Bearer test")
        .body(r#"{"stream":true,"model":"gpt-4"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();

    let mut parser = suture_sse::SseParser::new();
    let mut args = String::new();
    for data in parser.push(text.as_bytes()) {
        if data == b"[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
            if let Some(a) =
                v["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
            {
                args.push_str(a);
            }
        }
    }
    assert_eq!(args, r#"{"city":"Par"}"#);
    serde_json::from_str::<serde_json::Value>(&args).expect("repaired args must parse");
    assert!(text.contains("[DONE]"), "terminator appended");
}

#[tokio::test]
async fn proxies_and_repairs_truncated_json_body() {
    let up = spawn(Router::new().route("/v1/chat/completions", post(mock_json_truncated))).await;
    let cfg = Arc::new(Config::from_map(|k| match k {
        "SUTURE_OPENAI_BASE" => Some(up.clone()),
        _ => None,
    }));
    let proxy_url = spawn(proxy::app(cfg)).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/chat/completions"))
        .body(r#"{"stream":false}"#)
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert_eq!(body, r#"{"id":42,"text":"hello"}"#);
    serde_json::from_str::<serde_json::Value>(&body).expect("repaired body must parse");
}

async fn echo_accept_encoding(headers: HeaderMap) -> Response {
    let v = headers
        .get("accept-encoding")
        .map(|h| h.to_str().unwrap_or("").to_string())
        .unwrap_or_else(|| "ABSENT".to_string());
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!("{{\"ae\":\"{v}\"}}")))
        .unwrap()
}

async fn mock_unknown_encoded() -> Response {
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CONTENT_ENCODING, "zstd")
        .body(Body::from("RAWBYTES"))
        .unwrap()
}

async fn mock_gzip_truncated_sse() -> Response {
    let sse = "data: {\"id\":\"c1\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\\\"Par\"}}]}}]}\n\n";
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(sse.as_bytes()).unwrap();
    let gz = e.finish().unwrap();
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CONTENT_ENCODING, "gzip")
        .body(Body::from(gz))
        .unwrap()
}

#[tokio::test]
async fn forwards_accept_encoding_to_upstream() {
    let up = spawn(Router::new().route("/v1/chat/completions", post(echo_accept_encoding))).await;
    let cfg = std::sync::Arc::new(Config::from_map(|k| match k {
        "SUTURE_OPENAI_BASE" => Some(up.clone()),
        _ => None,
    }));
    let proxy_url = spawn(proxy::app(cfg)).await;
    let body = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("accept-encoding", "gzip")
        .body("{}")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, r#"{"ae":"gzip"}"#);
}

#[tokio::test]
async fn passes_through_unknown_encoding_unchanged() {
    let up = spawn(Router::new().route("/v1/chat/completions", post(mock_unknown_encoded))).await;
    let cfg = std::sync::Arc::new(Config::from_map(|k| match k {
        "SUTURE_OPENAI_BASE" => Some(up.clone()),
        _ => None,
    }));
    let proxy_url = spawn(proxy::app(cfg)).await;
    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/chat/completions"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers().get("content-encoding").unwrap(), "zstd");
    assert_eq!(&resp.bytes().await.unwrap()[..], b"RAWBYTES");
}

#[tokio::test]
async fn repairs_gzip_compressed_sse_end_to_end() {
    let up = spawn(Router::new().route("/v1/chat/completions", post(mock_gzip_truncated_sse))).await;
    let cfg = std::sync::Arc::new(Config::from_map(|k| match k {
        "SUTURE_OPENAI_BASE" => Some(up.clone()),
        _ => None,
    }));
    let proxy_url = spawn(proxy::app(cfg)).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("accept-encoding", "gzip")
        .body(r#"{"stream":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers().get("content-encoding").map(|v| v.to_str().unwrap()), Some("gzip"));
    let gz = resp.bytes().await.unwrap();

    // gunzip (our reqwest has no auto-decompress)
    let mut d = flate2::read::GzDecoder::new(&gz[..]);
    let mut text = String::new();
    std::io::Read::read_to_string(&mut d, &mut text).unwrap();

    let mut parser = suture_sse::SseParser::new();
    let mut args = String::new();
    for data in parser.push(text.as_bytes()) {
        if data == b"[DONE]" { continue; }
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
            if let Some(a) = v["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str() {
                args.push_str(a);
            }
        }
    }
    assert_eq!(args, r#"{"city":"Par"}"#);
    serde_json::from_str::<serde_json::Value>(&args).expect("repaired args must parse");
}

async fn mock_vertex_claude_truncated(headers: axum::http::HeaderMap) -> Response {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let sse = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":[1,2\"}}\n\n";
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header("x-seen-auth", auth)
        .body(Body::from(sse))
        .unwrap()
}

async fn mock_vertex_gemini_truncated() -> Response {
    let sse = "data: {\"candidates\":[{\"index\":0,\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"{\\\"city\\\":\\\"Par\"}]}}]}\n\n";
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(sse))
        .unwrap()
}

fn vertex_cfg(upstream: &str) -> std::sync::Arc<Config> {
    std::sync::Arc::new(Config::from_map(|k| match k {
        "SUTURE_VERTEX_ENABLED" => Some("1".to_string()),
        "SUTURE_VERTEX_BASE" => Some(upstream.to_string()),
        _ => None,
    }))
}

#[tokio::test]
async fn vertex_claude_repaired_and_forwards_bearer() {
    let up = spawn(Router::new().route("/v1/projects/*rest", post(mock_vertex_claude_truncated))).await;
    let proxy_url = spawn(proxy::app(vertex_cfg(&up))).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/projects/p/locations/us-central1/publishers/anthropic/models/claude:streamRawPredict"))
        .header("authorization", "Bearer ya29.test-token")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers().get("x-seen-auth").unwrap(), "Bearer ya29.test-token");
    let text = resp.text().await.unwrap();

    let mut parser = suture_sse::SseParser::new();
    let mut pj = String::new();
    for data in parser.push(text.as_bytes()) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
            if v["delta"]["type"] == "input_json_delta" {
                if let Some(s) = v["delta"]["partial_json"].as_str() {
                    pj.push_str(s);
                }
            }
        }
    }
    assert_eq!(pj, r#"{"a":[1,2]}"#);
    serde_json::from_str::<serde_json::Value>(&pj).expect("repaired input must parse");
}

#[tokio::test]
async fn vertex_gemini_json_text_repaired() {
    let up = spawn(Router::new().route("/v1/projects/*rest", post(mock_vertex_gemini_truncated))).await;
    let proxy_url = spawn(proxy::app(vertex_cfg(&up))).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/projects/p/locations/us-central1/publishers/google/models/gemini:streamGenerateContent?alt=sse"))
        .header("authorization", "Bearer ya29.x")
        .body("{}")
        .send()
        .await
        .unwrap();
    let text = resp.text().await.unwrap();

    let mut parser = suture_sse::SseParser::new();
    let mut content = String::new();
    for data in parser.push(text.as_bytes()) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
            if let Some(parts) = v["candidates"][0]["content"]["parts"].as_array() {
                for p in parts {
                    if let Some(t) = p["text"].as_str() {
                        content.push_str(t);
                    }
                }
            }
        }
    }
    assert_eq!(content, r#"{"city":"Par"}"#);
    serde_json::from_str::<serde_json::Value>(&content).expect("repaired text must parse");
}

#[tokio::test]
async fn health_returns_ok() {
    let cfg = std::sync::Arc::new(Config::from_map(|_| None));
    let proxy_url = spawn(proxy::app(cfg)).await;
    let resp = reqwest::Client::new()
        .get(format!("{proxy_url}/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");
}
