/// Optional bearer-token auth middleware.
///
/// When an API key is configured, every request must carry:
///   Authorization: Bearer <key>
///
/// The check is constant-time to prevent timing attacks.
use axum::{
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

#[derive(Clone)]
pub struct AuthState {
    /// Pre-hashed expected key bytes. None = auth disabled.
    expected: Option<Vec<u8>>,
}

impl AuthState {
    pub fn new(api_key: Option<&str>) -> Self {
        Self {
            expected: api_key.map(|k| k.as_bytes().to_vec()),
        }
    }

    #[allow(dead_code)]
    pub fn enabled(&self) -> bool {
        self.expected.is_some()
    }
}

pub async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<AuthState>,
    req: Request,
    next: Next,
) -> Response {
    if let Some(expected) = &state.expected {
        let provided = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|v| v.as_bytes().to_vec())
            .unwrap_or_default();

        if !constant_time_eq(&provided, expected) {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    }
    next.run(req).await
}

/// Constant-time byte comparison (prevents timing-based token oracle).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_correct() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
    }
}
