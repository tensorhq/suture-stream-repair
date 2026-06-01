use std::sync::Arc;

use suture::{config::Config, proxy};

#[tokio::main]
async fn main() {
    let cfg = Arc::new(Config::from_env());
    let listen = cfg.listen;
    let app = proxy::app(cfg);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {listen}: {e}"));
    eprintln!("suture proxy listening on http://{listen}");
    eprintln!("  /v1/chat/completions -> OpenAI    /v1/messages -> Anthropic");
    axum::serve(listener, app).await.expect("server error");
}
