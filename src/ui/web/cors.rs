use axum::http::{HeaderValue, Method, Request, header};
use axum::middleware::Next;
use axum::response::Response;

/// CORS middleware: allow cross-origin requests so that a standalone HTML page
/// (e.g. `unified.html` opened via `file://` or served by a different host)
/// can call this server's API endpoints.
///
/// Auth is handled via the `X-Tomato-Password` request header (already supported
/// by the auth middleware), so we don't need `Access-Control-Allow-Credentials`.
pub(crate) async fn cors_mw(req: Request<axum::body::Body>, next: Next) -> Response {
    // Handle preflight OPTIONS request
    if req.method() == Method::OPTIONS {
        let mut resp = Response::new(axum::body::Body::empty());
        *resp.status_mut() = axum::http::StatusCode::NO_CONTENT;
        set_cors_headers(resp.headers_mut());
        return resp;
    }

    let mut resp = next.run(req).await;
    set_cors_headers(resp.headers_mut());
    resp
}

fn set_cors_headers(headers: &mut axum::http::HeaderMap) {
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, DELETE, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, X-Tomato-Password"),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("86400"),
    );
}
