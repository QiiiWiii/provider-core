use std::convert::Infallible;
use std::path::{Path, PathBuf};

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use tower::{ServiceExt, service_fn, util::BoxCloneSyncService};
use tower_http::services::{ServeDir, ServeFile};

/// Serve compiled UI assets and fall back to `index.html` for browser routes.
pub(super) fn ui_service(
    public_dir: impl AsRef<Path>,
) -> BoxCloneSyncService<Request<Body>, Response, Infallible> {
    let public_dir = public_dir.as_ref();
    let files = ServeDir::new(public_dir);
    let index = public_dir.join("index.html");
    BoxCloneSyncService::new(service_fn(move |request| {
        serve_ui(request, files.clone(), index.clone())
    }))
}

async fn serve_ui(
    request: Request<Body>,
    files: ServeDir,
    index: PathBuf,
) -> Result<Response, Infallible> {
    if !matches!(*request.method(), Method::GET | Method::HEAD)
        || is_backend_path(request.uri().path())
    {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }

    let accepts_html = accepts_html(request.headers());
    let method = request.method().clone();
    let response = into_axum_response(files.oneshot(request).await);
    if response.status() != StatusCode::NOT_FOUND || !accepts_html {
        return Ok(response);
    }

    let request = Request::builder()
        .method(method)
        .uri("/")
        .body(Body::empty())
        .expect("static fallback request is valid");
    Ok(into_axum_response(
        ServeFile::new(index).oneshot(request).await,
    ))
}

fn into_axum_response<T>(response: Result<T, Infallible>) -> Response
where
    T: IntoResponse,
{
    response
        .map(IntoResponse::into_response)
        .unwrap_or_else(|never| match never {})
}

fn is_backend_path(path: &str) -> bool {
    ["/api", "/v1", "/healthz", "/livez", "/readyz"]
        .iter()
        .any(|prefix| {
            path == *prefix
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
}

fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .filter_map(|media_type| media_type.split(';').next())
                .any(|media_type| media_type.trim().eq_ignore_ascii_case("text/html"))
        })
}
