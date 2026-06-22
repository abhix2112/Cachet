//! The built-in dashboard, served on the reserved `/__cachet/` namespace.
//!
//! These routes are intercepted by the proxy *before* any forwarding logic, so they
//! are the one deliberate carve-out from full transparency and never reach upstream.
//!
//!   * `GET /__cachet/`        → the self-contained dashboard page (embedded HTML).
//!   * `GET /__cachet/events`  → a Server-Sent Events feed of live metrics.
//!   * `GET /__cachet/stats`   → alias of `/events`.
//!
//! The page is `include_str!`'d into the binary, so there are no static files to
//! ship and no external/CDN requests — it works fully offline.

use axum::body::Body;
use axum::http::{header, Method, Response, StatusCode};
use bytes::Bytes;
use futures_util::StreamExt;
use tokio::sync::broadcast::error::RecvError;

use crate::error::ProxyError;
use crate::proxy::AppState;

/// The entire dashboard UI, embedded at compile time.
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// Route a reserved `/__cachet*` request. Returns 404 for unknown sub-paths and
/// 405 for non-GET methods — but in every case the request is handled here and is
/// **never forwarded upstream**.
pub async fn handle(
    state: &AppState,
    method: &Method,
    path: &str,
) -> Result<Response<Body>, ProxyError> {
    match path {
        "/__cachet" | "/__cachet/" => page(method),
        "/__cachet/events" | "/__cachet/stats" => events(method, state),
        _ => simple(StatusCode::NOT_FOUND, "unknown cachet endpoint"),
    }
}

fn page(method: &Method) -> Result<Response<Body>, ProxyError> {
    if method != Method::GET {
        return simple(StatusCode::METHOD_NOT_ALLOWED, "use GET");
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(DASHBOARD_HTML))
        .map_err(|e| ProxyError::Internal(e.to_string()))
}

/// The live SSE feed: an immediate snapshot, then one event per recorded request.
fn events(method: &Method, state: &AppState) -> Result<Response<Body>, ProxyError> {
    if method != Method::GET {
        return simple(StatusCode::METHOD_NOT_ALLOWED, "use GET");
    }

    let snapshot = state.metrics.snapshot_message();
    let rx = state.metrics.subscribe();

    // First frame: a full snapshot so a freshly-opened dashboard is populated.
    let initial = futures_util::stream::once(async move {
        Ok::<Bytes, std::convert::Infallible>(sse_frame(&snapshot))
    });
    // Then: live events from the broadcast channel. A lagged subscriber simply
    // skips the rows it missed and keeps going; a closed channel ends the stream.
    let live = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(msg) => return Some((Ok::<Bytes, std::convert::Infallible>(sse_frame(&msg)), rx)),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return None,
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        // Hint to any intermediary proxies not to buffer the event stream.
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(initial.chain(live)))
        .map_err(|e| ProxyError::Internal(e.to_string()))
}

fn sse_frame(json: &str) -> Bytes {
    Bytes::from(format!("data: {json}\n\n"))
}

fn simple(status: StatusCode, message: &str) -> Result<Response<Body>, ProxyError> {
    let body = format!(
        r#"{{"error":{{"type":"cachet_endpoint","message":"{message}","proxy":"{}"}}}}"#,
        crate::PROJECT_SLUG
    );
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .map_err(|e| ProxyError::Internal(e.to_string()))
}
