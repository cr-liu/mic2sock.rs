use axum::{
    extract::State,
    http::{header, StatusCode, Request},
    middleware::Next,
    response::Response,
    body::Body,
};
use base64::{engine::general_purpose, Engine as _};
use subtle::ConstantTimeEq;
use std::sync::Arc;

pub struct AuthState {
    pub password: Arc<String>,
}

pub async fn auth_middleware(
    State(state): State<Arc<AuthState>>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if state.password.is_empty() {
        return Ok(next.run(req).await);
    }

    if let Some(auth) = req.headers().get(header::AUTHORIZATION) {
        if let Ok(s) = auth.to_str() {
            if let Some(b64) = s.strip_prefix("Basic ") {
                if let Ok(decoded) = general_purpose::STANDARD.decode(b64) {
                    if let Ok(text) = std::str::from_utf8(&decoded) {
                        if let Some((_user, pass)) = text.split_once(':') {
                            let pw_bytes = pass.as_bytes();
                            let expected = state.password.as_bytes();
                            if pw_bytes.len() == expected.len()
                                && pw_bytes.ct_eq(expected).into()
                            {
                                return Ok(next.run(req).await);
                            }
                        }
                    }
                }
            }
        }
    }

    let mut response = Response::new(Body::from("Unauthorized"));
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        "Basic realm=\"mic2sock\"".parse().unwrap(),
    );
    Ok(response)
}
