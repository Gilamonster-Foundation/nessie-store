//! HTTP Basic authentication middleware.
//!
//! ONTAP clients authenticate with HTTP Basic on every request. This mirrors
//! the predecessor's behavior: a constant-time credential compare, a bypass for
//! the documentation paths, and a `401` carrying `WWW-Authenticate: Basic
//! realm="ONTAP"` plus the ONTAP-native error envelope (so SDKs parse it).

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::json;

use crate::state::AppState;

/// Paths served without authentication (OpenAPI docs), matching ONTAP/the sim.
const BYPASS: &[&str] = &["/docs", "/openapi.json", "/redoc"];

/// Constant-time byte comparison (avoids leaking credential length/content via
/// timing). Returns false fast only on length mismatch.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn unauthorized(message: &str) -> Response {
    let body = Json(json!({
        "error": { "code": "401", "message": message, "target": "authentication" }
    }));
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"ONTAP\"")],
        body,
    )
        .into_response()
}

/// Axum middleware enforcing HTTP Basic auth against the configured credentials.
pub async fn require_basic_auth(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if BYPASS.contains(&req.uri().path()) {
        return next.run(req).await;
    }

    let Some(value) = req.headers().get(header::AUTHORIZATION) else {
        return unauthorized("Authentication required");
    };
    let Ok(value) = value.to_str() else {
        return unauthorized("Invalid credentials");
    };
    let Some(b64) = value.strip_prefix("Basic ") else {
        return unauthorized("Authentication required");
    };
    let Ok(decoded) = STANDARD.decode(b64.trim()) else {
        return unauthorized("Invalid credentials");
    };
    let Ok(creds) = String::from_utf8(decoded) else {
        return unauthorized("Invalid credentials");
    };
    let Some((user, pass)) = creds.split_once(':') else {
        return unauthorized("Invalid credentials");
    };

    let user_ok = ct_eq(user.as_bytes(), state.config.admin_username.as_bytes());
    let pass_ok = ct_eq(pass.as_bytes(), state.config.admin_password.as_bytes());
    if user_ok && pass_ok {
        next.run(req).await
    } else {
        unauthorized("Invalid credentials")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_matches_and_rejects() {
        assert!(ct_eq(b"admin", b"admin"));
        assert!(!ct_eq(b"admin", b"admiN"));
        assert!(!ct_eq(b"admin", b"administrator"));
    }
}
