use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};

use crate::ui::web::templates;

pub(crate) async fn index() -> impl IntoResponse {
    let html = templates::INDEX_HTML_RAW.replace("{{FREE_NOTICE}}", templates::FREE_NOTICE_HTML);
    let mut resp = Html(html).into_response();
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    resp.headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    resp.headers_mut()
        .insert(header::EXPIRES, HeaderValue::from_static("0"));
    resp
}

pub(crate) async fn asset_css() -> Response {
    let mut resp = Response::new(templates::APP_CSS.into());
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/css; charset=utf-8"),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    resp.headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    resp.headers_mut()
        .insert(header::EXPIRES, HeaderValue::from_static("0"));
    resp
}

pub(crate) async fn asset_js() -> Response {
    let mut resp = Response::new(templates::APP_JS.into());
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/javascript; charset=utf-8"),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    resp.headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    resp.headers_mut()
        .insert(header::EXPIRES, HeaderValue::from_static("0"));
    resp
}

pub(crate) async fn asset_favicon_ico() -> Response {
    icon_response(templates::APP_FAVICON_ICO, "image/x-icon")
}

pub(crate) async fn asset_icon_fqnovel() -> Response {
    icon_response(templates::ICON_FQNOVEL, "image/webp")
}

pub(crate) async fn asset_icon_sqnovel() -> Response {
    icon_response(templates::ICON_SQNOVEL, "image/png")
}

pub(crate) async fn asset_icon_qmnovel() -> Response {
    icon_response(templates::ICON_QMNOVEL, "image/webp")
}

fn icon_response(data: &[u8], content_type: &str) -> Response {
    let mut resp = Response::new(data.to_vec().into());
    *resp.status_mut() = StatusCode::OK;
    if let Ok(val) = HeaderValue::from_str(content_type) {
        resp.headers_mut().insert(header::CONTENT_TYPE, val);
    }
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    resp
}
