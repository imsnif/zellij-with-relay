//! Axum router and shared application state.

use axum::{
    routing::{get, any},
    Router,
};

use crate::registry::Registry;

#[derive(Clone)]
pub struct AppState {
    pub registry: Registry,
    pub public_url_template: String,
}

impl AppState {
    pub fn new(public_url_template: String) -> Self {
        Self {
            registry: Registry::new(),
            public_url_template,
        }
    }

    pub fn render_public_url(&self, slug: &str) -> String {
        crate::config::format_public_url(&self.public_url_template, slug)
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/tunnel/control", any(crate::tunnel_control::handler))
        .route("/tunnel/terminal", any(crate::tunnel_terminal::handler))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}
