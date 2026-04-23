//! `zellij-relay` — tunnel relay server for Zellij remote session sharing.
//!
//! Phase 6 Session C: the default (no subcommand) invocation runs the relay
//! server as before. New `create-token` / `revoke-token` / `list-tokens`
//! subcommands manage the tunnel-auth token store — see
//! `relay_tunnel_auth_tokens` for schema and storage details.

use std::net::SocketAddr;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use zellij_relay::cli::{Cli, Command};
use zellij_relay::relay_tunnel_auth_tokens::{
    list_relay_tunnel_auth_tokens, revoke_relay_tunnel_auth_token,
    store_new_relay_tunnel_auth_token,
};
use zellij_relay::{config, router};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.resolved_command() {
        Command::Serve => run_server(),
        Command::CreateToken { label } => run_create_token(label),
        Command::RevokeToken { label_or_token } => run_revoke_token(&label_or_token),
        Command::ListTokens => run_list_tokens(),
    }
}

fn run_server() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    runtime.block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
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

fn run_create_token(label: Option<String>) -> anyhow::Result<()> {
    let token = store_new_relay_tunnel_auth_token(label.clone())
        .with_context(|| "failed to create relay tunnel auth token")?;
    match label {
        Some(l) => println!(
            "Relay tunnel auth token created (label: {}). Store this token securely — it will not be shown again:",
            l
        ),
        None => println!(
            "Relay tunnel auth token created. Store this token securely — it will not be shown again:"
        ),
    }
    println!("{}", token);
    Ok(())
}

fn run_revoke_token(label_or_token: &str) -> anyhow::Result<()> {
    let removed = revoke_relay_tunnel_auth_token(label_or_token)
        .with_context(|| format!("failed to revoke '{label_or_token}'"))?;
    println!("Revoked {removed} relay tunnel auth token(s).");
    Ok(())
}

fn run_list_tokens() -> anyhow::Result<()> {
    let tokens = list_relay_tunnel_auth_tokens().context("failed to list relay tunnel auth tokens")?;
    if tokens.is_empty() {
        println!("No relay tunnel auth tokens configured.");
        return Ok(());
    }
    println!("{:<32} {:<20} {:<20}", "LABEL", "CREATED_AT", "LAST_USED_AT");
    for t in tokens {
        let label = t.label.as_deref().unwrap_or("<unnamed>");
        let last_used = t
            .last_used_at
            .map(|v| v.to_string())
            .unwrap_or_else(|| "<never>".into());
        println!("{:<32} {:<20} {:<20}", label, t.created_at, last_used);
    }
    Ok(())
}
