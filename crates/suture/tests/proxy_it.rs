use std::sync::Arc;

use axum::{body::Body, http::{header, HeaderMap}, response::Response, routing::post, Router};
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

async fn mock_encoded_sse() -> Response {
    // Pretend-encoded SSE body: the guard must passthrough unchanged (no synthetic [DONE]).
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CONTENT_ENCODING, "gzip")
        .body(Body::from("RAWBYTES"))
        .unwrap()
}

#[tokio::test]
async fn strips_accept_encoding_to_upstream() {
    let up = spawn(Router::new().route("/v1/chat/completions", post(echo_accept_encoding))).await;
    let cfg = std::sync::Arc::new(Config::from_map(|k| match k {
        "SUTURE_OPENAI_BASE" => Some(up.clone()),
        _ => None,
    }));
    let proxy_url = spawn(proxy::app(cfg)).await;
    let body = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("accept-encoding", "gzip, br")
        .body("{}")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, r#"{"ae":"ABSENT"}"#, "Accept-Encoding must be stripped before upstream");
}

#[tokio::test]
async fn passes_through_encoded_response_unchanged() {
    let up = spawn(Router::new().route("/v1/chat/completions", post(mock_encoded_sse))).await;
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
    // reqwest may or may not auto-decode; disable by checking the raw header + bytes.
    let ce = resp.headers().get("content-encoding").map(|v| v.to_str().unwrap().to_string());
    let bytes = resp.bytes().await.unwrap();
    assert_eq!(ce.as_deref(), Some("gzip"), "Content-Encoding preserved");
    assert_eq!(&bytes[..], b"RAWBYTES", "encoded body must pass through unchanged (no synthetic [DONE])");
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
