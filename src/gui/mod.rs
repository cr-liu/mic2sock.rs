mod auth;
mod config_api;
mod waveform;

use axum::{
    Router,
    routing::{get, post},
    response::Response,
    http::header,
    middleware,
};
use std::sync::Arc;
use std::net::SocketAddr;
use std::collections::HashSet;
use arc_swap::ArcSwap;
use tokio::sync::broadcast;

use crate::config_file::GuiConfig;

const INDEX_HTML: &str = include_str!("assets/index.html");
const APP_JS: &str = include_str!("assets/app.js");
const STYLE_CSS: &str = include_str!("assets/style.css");

async fn serve_html() -> Response<String> {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(INDEX_HTML.to_string())
        .unwrap()
}

async fn serve_js() -> Response<String> {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/javascript; charset=utf-8")
        .body(APP_JS.to_string())
        .unwrap()
}

async fn serve_css() -> Response<String> {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/css; charset=utf-8")
        .body(STYLE_CSS.to_string())
        .unwrap()
}

pub struct GuiHandles {
    pub static_set: Arc<ArcSwap<HashSet<SocketAddr>>>,
    pub waveform_tap: broadcast::Sender<Vec<i16>>,
    pub config_path: std::path::PathBuf,
}

pub async fn start_gui(
    cfg: GuiConfig,
    handles: Arc<GuiHandles>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !cfg.enabled {
        println!("GUI disabled by config");
        return Ok(());
    }

    if cfg.password.is_empty() && cfg.bind_addr == "0.0.0.0" {
        eprintln!("WARNING: GUI bound to 0.0.0.0 without password");
    }

    let auth_state = Arc::new(auth::AuthState {
        password: Arc::new(cfg.password.clone()),
    });

    let api_routes: Router = Router::new()
        .route(
            "/api/config",
            get(config_api::get_config).post(config_api::post_config),
        )
        .route("/api/restart", post(config_api::post_restart))
        .route("/ws/waveform", get(waveform::ws_waveform))
        .with_state(handles);

    let app = Router::new()
        .route("/", get(serve_html))
        .route("/app.js", get(serve_js))
        .route("/style.css", get(serve_css))
        .merge(api_routes)
        .layer(tower_http::limit::RequestBodyLimitLayer::new(64 * 1024))
        .layer(middleware::from_fn_with_state(auth_state, auth::auth_middleware));

    let addr: std::net::SocketAddr = format!("{}:{}", cfg.bind_addr, cfg.port).parse()?;
    println!("GUI listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
