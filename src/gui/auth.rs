use axum::{
    extract::State,
    http::{header, StatusCode, Request},
    middleware::Next,
    response::{Response, IntoResponse, Redirect},
    body::Body,
};
use subtle::ConstantTimeEq;
use std::sync::Arc;

pub struct AuthState {
    pub password: Arc<String>,
    /// Session token that a valid cookie must match. Random per-startup.
    pub token: Arc<String>,
}

pub async fn auth_middleware(
    State(state): State<Arc<AuthState>>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if state.password.is_empty() {
        return Ok(next.run(req).await);
    }

    let path = req.uri().path();

    // Always allow the login endpoints (GET form, POST submit) and static login asset.
    if path == "/login" || path == "/login.html" {
        return Ok(next.run(req).await);
    }

    if cookie_is_valid(req.headers(), &state.token) {
        return Ok(next.run(req).await);
    }

    // Distinguish browser navigation vs API/WebSocket requests.
    let is_api = path.starts_with("/api/") || path.starts_with("/ws/");

    if is_api {
        let mut response = Response::new(Body::from("Unauthorized"));
        *response.status_mut() = StatusCode::UNAUTHORIZED;
        Ok(response)
    } else {
        Ok(Redirect::to("/login").into_response())
    }
}

fn cookie_is_valid(headers: &axum::http::HeaderMap, expected: &str) -> bool {
    let Some(cookie_header) = headers.get(header::COOKIE) else {
        return false;
    };
    let Ok(text) = cookie_header.to_str() else {
        return false;
    };
    for pair in text.split(';') {
        let pair = pair.trim();
        if let Some(value) = pair.strip_prefix("auth=") {
            if value.len() == expected.len()
                && value.as_bytes().ct_eq(expected.as_bytes()).into()
            {
                return true;
            }
        }
    }
    false
}
