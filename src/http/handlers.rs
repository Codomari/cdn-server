use axum::{
    extract::{OriginalUri, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use url::Url;

use crate::{
    cache::CachedResponse,
    error::RequestError,
    upstream::{self, Upstream},
};

use super::{App, file_response};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FetchQuery {
    url: String,
}

pub(super) async fn fetch(
    State(app): State<App>,
    Query(query): Query<FetchQuery>,
) -> Result<StatusCode, RequestError> {
    let url = upstream::parse(&query.url)?;
    cached_entry(&app, url).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn serve(
    State(app): State<App>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Result<Response, RequestError> {
    let url = origin_url(&uri, "/serve/")?;
    let cached = cached_entry(&app, url).await?;
    match cached.entry {
        Upstream::File(content_type, bytes) => Ok(file_response::serve_file(
            &headers,
            content_type,
            bytes,
            &cached.original_name,
            cached.expires_at,
            cached.version_mismatch,
        )),
        Upstream::Redirect(redirect) => {
            let status =
                StatusCode::from_u16(redirect.status).map_err(|_| RequestError::RedirectDenied)?;
            let location = HeaderValue::from_str(&redirect.location)
                .map_err(|_| RequestError::RedirectDenied)?;
            let mut response = (status, [(header::LOCATION, location)]).into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            Ok(response)
        }
    }
}

pub(super) async fn download(
    State(app): State<App>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response, RequestError> {
    let url = origin_url(&uri, "/download/")?;
    let mut cached = cached_entry(&app, url).await?;
    if let Upstream::Redirect(redirect) = &cached.entry {
        let url = upstream::parse(&redirect.location).map_err(|_| RequestError::RedirectDenied)?;
        cached = cached_entry(&app, url).await?;
    }
    match cached.entry {
        Upstream::File(_, bytes) => Ok(file_response::download_file(bytes, &cached.original_name)),
        Upstream::Redirect(_) => Err(RequestError::RedirectDenied),
    }
}

fn origin_url(uri: &Uri, prefix: &str) -> Result<Url, RequestError> {
    let target = uri
        .path_and_query()
        .and_then(|value| value.as_str().strip_prefix(prefix))
        .ok_or(RequestError::InvalidUrl)?;
    if target.starts_with(['/', '\\', '?']) {
        return Err(RequestError::InvalidUrl);
    }
    upstream::parse(&format!("https://{target}"))
}

async fn cached_entry(app: &App, url: Url) -> Result<CachedResponse, RequestError> {
    let _permit = app
        .permits
        .try_acquire()
        .map_err(|_| RequestError::TooManyRequests)?;
    app.cache.get_or_download(url).await
}
