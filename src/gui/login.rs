use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use subtle::ConstantTimeEq;
use std::sync::Arc;

use super::auth::AuthState;

const LOGIN_HTML: &str = include_str!("assets/login.html");

pub async fn get_login() -> Response {
    axum::response::Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(axum::body::Body::from(LOGIN_HTML))
        .unwrap()
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

pub async fn post_login(
    State(state): State<Arc<AuthState>>,
    Json(body): Json<LoginRequest>,
) -> Response {
    let pw = body.password.as_bytes();
    let expected = state.password.as_bytes();
    let ok = pw.len() == expected.len() && pw.ct_eq(expected).into();
    if !ok {
        return (StatusCode::UNAUTHORIZED, "Wrong password").into_response();
    }

    let cookie = format!(
        "auth={}; HttpOnly; Path=/; SameSite=Lax; Max-Age=31536000",
        state.token
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::SET_COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(r#"{"status":"ok"}"#))
        .unwrap()
}
