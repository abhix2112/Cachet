//! Proxy error type and its HTTP representation.
//!
//! Every failure surfaces as a clean JSON body instead of a panic or an empty
//! connection drop, so clients always get a structured, debuggable response.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// We could not reach the upstream, or the connection failed mid-flight
    /// (DNS failure, refused connection, TLS error, connect timeout, ...).
    #[error("upstream request failed: {0}")]
    Upstream(#[from] reqwest::Error),

    /// The incoming request body could not be read (e.g. exceeded the size cap).
    #[error("could not read request body: {0}")]
    InvalidRequestBody(String),

    /// Something went wrong building the response we hand back to the client.
    #[error("internal proxy error: {0}")]
    Internal(String),
}

impl ProxyError {
    /// Status, machine-readable type, and a **safe, generic** client-facing message.
    /// The message is deliberately fixed per-variant — we never echo the underlying
    /// error (e.g. a reqwest error embeds the upstream URL, which can contain a
    /// query-string API key) back to the client. Full detail is logged server-side.
    fn parts(&self) -> (StatusCode, &'static str, &'static str) {
        match self {
            // Bad Gateway: the proxy is fine, the upstream is the problem.
            ProxyError::Upstream(_) => (
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                "upstream request failed",
            ),
            ProxyError::InvalidRequestBody(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "could not read request body",
            ),
            ProxyError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal proxy error",
            ),
        }
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let (status, kind, message) = self.parts();
        // Return only the generic message to the client. Log detail server-side —
        // but for an upstream error, do NOT log the reqwest `Display` (it embeds the
        // full URL, which can carry a query-string credential); the connection-level
        // detail is logged with a host + classification in `dispatch_upstream`.
        match &self {
            ProxyError::Upstream(_) => tracing::debug!(kind, "returning error response"),
            other => tracing::debug!(error = %other, kind, "returning error response"),
        }
        let body = Json(json!({
            "error": {
                "type": kind,
                "message": message,
                "proxy": crate::PROJECT_SLUG,
            }
        }));
        (status, body).into_response()
    }
}
