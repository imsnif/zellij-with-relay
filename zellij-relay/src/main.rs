//! `zellij-relay` — tunnel relay server for Zellij remote session sharing.
//!
//! Phase 1 scope: accept outbound WebSocket tunnels from Zellij instances,
//! allocate a slug, and report a public URL back. There is no HTTP surface for
//! remote viewers yet — that arrives in Phase 2.

use std::net::SocketAddr;

use anyhow::Context;
use tracing_subscriber::EnvFilter;

use zellij_relay::{config, router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = config::RelayConfig::from_env();
    tracing::info!(
        bind = %cfg.bind_addr,
        public_url_template = %cfg.public_url_template,
        "starting zellij-relay"
    );

    let app_state = router::AppState::new(cfg.public_url_template.clone());
    let app = router::build_router(app_state);

    let addr: SocketAddr = cfg
        .bind_addr
        .parse()
        .with_context(|| format!("invalid bind address {:?}", cfg.bind_addr))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    tracing::info!(%addr, "listening");

    axum::serve(listener, app.into_make_service())
        .await
        .context("axum serve failed")?;
    Ok(())
}
