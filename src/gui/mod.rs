mod auth;
mod config_api;
mod login;
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
use std::time::{SystemTime, UNIX_EPOCH};
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

/// Minimal token generator — reads /dev/urandom on Unix, or hashes high-res
/// timestamp on other platforms as a fallback. Sufficient for LAN auth.
fn generate_token() -> String {
    let mut buf = [0u8; 32];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| {
            use std::io::Read;
            f.read_exact(&mut buf)
        })
        .is_err()
    {
        // Fallback: mix nanos with addr of local for uniqueness
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let local_ptr = &buf as *const _ as usize;
        let mix = nanos ^ (local_ptr as u128);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((mix >> (i * 4)) & 0xff) as u8;
        }
    }
    buf.iter().map(|b| format!("{:02x}", b)).collect()
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
        token: Arc::new(generate_token()),
    });

    let login_routes: Router = Router::new()
        .route("/login", get(login::get_login).post(login::post_login))
        .with_state(auth_state.clone());

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
        .merge(login_routes)
        .merge(api_routes)
        .layer(tower_http::limit::RequestBodyLimitLayer::new(64 * 1024))
        .layer(middleware::from_fn_with_state(auth_state, auth::auth_middleware));

    let addr: std::net::SocketAddr = format!("{}:{}", cfg.bind_addr, cfg.port).parse()?;
    println!("GUI listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
