use axum::{
    extract::State,
    http::StatusCode,
    Json,
    response::IntoResponse,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::io::Write;

use crate::config_file::Config;
use super::GuiHandles;

pub async fn get_config(State(handles): State<Arc<GuiHandles>>) -> impl IntoResponse {
    let contents = match std::fs::read_to_string(&handles.config_path) {
        Ok(s) => s,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("read error: {}", e))
                .into_response()
        }
    };
    let mut cfg: Value = match toml::from_str(&contents) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("parse error: {}", e))
                .into_response()
        }
    };
    if let Some(obj) = cfg.as_object_mut() {
        obj.insert(
            "_meta".to_string(),
            json!({ "hot_reloadable_fields": ["sender.static_receivers"] }),
        );
    }
    Json(cfg).into_response()
}

pub async fn post_config(
    State(handles): State<Arc<GuiHandles>>,
    Json(new_val): Json<Value>,
) -> impl IntoResponse {
    // Strip any _meta field the client might have sent back
    let mut clean = new_val.clone();
    if let Some(obj) = clean.as_object_mut() {
        obj.remove("_meta");
    }

    let toml_string = match toml::to_string_pretty(&clean) {
        Ok(s) => s,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("serialize error: {}", e))
                .into_response()
        }
    };
    let new_cfg: Config = match toml::from_str(&toml_string) {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("validate error: {}", e))
                .into_response()
        }
    };

    // Read previous config for diff
    let prev_toml = std::fs::read_to_string(&handles.config_path).unwrap_or_default();
    let prev_cfg: Option<Config> = toml::from_str(&prev_toml).ok();

    // Atomic write
    let dir = handles
        .config_path
        .parent()
        .unwrap_or(std::path::Path::new("."));
    let mut tmp = match tempfile::NamedTempFile::new_in(dir) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("tempfile error: {}", e),
            )
                .into_response()
        }
    };
    if let Err(e) = tmp.write_all(toml_string.as_bytes()) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("write error: {}", e))
            .into_response();
    }
    if let Err(e) = tmp.persist(&handles.config_path) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("persist error: {}", e),
        )
            .into_response();
    }

    // Hot reload static_receivers
    let new_statics: std::collections::HashSet<std::net::SocketAddr> = new_cfg
        .sender
        .static_receivers
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();
    handles.static_set.store(Arc::new(new_statics));

    // Diff for restart_required flag
    let restart_required = match &prev_cfg {
        Some(prev) => non_hot_fields_differ(prev, &new_cfg),
        None => true,
    };

    Json(json!({
        "status": if restart_required { "saved" } else { "applied" },
        "restart_required": restart_required,
    }))
    .into_response()
}

fn non_hot_fields_differ(a: &Config, b: &Config) -> bool {
    let mut a2 = a.clone();
    let mut b2 = b.clone();
    a2.sender.static_receivers.clear();
    b2.sender.static_receivers.clear();
    toml::to_string(&a2).ok() != toml::to_string(&b2).ok()
}

pub async fn post_restart() -> impl IntoResponse {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        std::process::exit(0);
    });
    Json(json!({ "status": "restarting" }))
}
