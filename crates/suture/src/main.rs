use std::sync::Arc;

use suture::{config::Config, proxy};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = Arc::new(Config::from_env());
    let listen = cfg.listen;
    let app = proxy::app(cfg);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {listen}: {e}"));
    tracing::info!(%listen, "suture proxy listening");
    tracing::info!("/v1/chat/completions -> OpenAI    /v1/messages -> Anthropic    /v1/projects/* -> Vertex    /model/* -> Bedrock");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}

/// Resolve on Ctrl-C or (on Unix) SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}
