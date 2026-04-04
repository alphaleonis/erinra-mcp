//! Bearer token authentication middleware for API routes.

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::AppState;

/// Middleware that validates the `Authorization: Bearer <token>` header
/// against the token stored in `AppState`. Returns 401 if missing or invalid.
pub async fn require_bearer_token(
    State(state): State<AppState>,
    req: axum::http::Request<Body>,
    next: Next,
) -> Response {
    let authorized = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| !token.is_empty() && token == state.auth_token);

    if authorized {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
        )
            .into_response()
    }
}

/// Generate a Bearer token. If `ERINRA_AUTH_TOKEN` is set, uses that value
/// (useful for testing). Otherwise generates a cryptographically random token
/// (32 bytes, hex-encoded -> 64 chars).
pub fn generate_auth_token() -> String {
    if let Ok(token) = std::env::var("ERINRA_AUTH_TOKEN")
        && !token.is_empty()
    {
        return token;
    }
    use rand::RngExt;
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_token_is_64_hex_chars() {
        let token = generate_auth_token();
        assert_eq!(token.len(), 64, "token should be 64 hex chars (32 bytes)");
        assert!(
            token.chars().all(|c| c.is_ascii_hexdigit()),
            "token should contain only hex characters, got: {token}"
        );
    }

    #[test]
    fn generated_tokens_are_unique() {
        let token1 = generate_auth_token();
        let token2 = generate_auth_token();
        assert_ne!(token1, token2, "two generated tokens should be different");
    }
}
