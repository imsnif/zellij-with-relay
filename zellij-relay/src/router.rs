//! Axum router and shared application state.

use axum::{
    routing::{any, get, post},
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
        .route("/r/{slug}", get(crate::viewer::serve_html))
        .route("/r/{slug}/info/version", get(crate::viewer::version))
        .route("/r/{slug}/session", post(crate::viewer::post_session))
        .route(
            "/r/{slug}/command/login",
            post(crate::viewer::post_login),
        )
        .route(
            "/r/{slug}/ws/terminal",
            any(crate::viewer::ws_terminal),
        )
        .route(
            "/r/{slug}/ws/terminal/{session}",
            any(crate::viewer::ws_terminal_with_session),
        )
        .route(
            "/r/{slug}/ws/control",
            any(crate::viewer::ws_control),
        )
        .route(
            "/r/{slug}/assets/{*path}",
            get(crate::viewer::serve_asset),
        )
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}
