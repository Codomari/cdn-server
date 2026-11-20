mod allowed_hosts;
mod cache;
mod config;
mod file_policy;
mod listeners;
mod storage;
mod upstream;

use std::{
    env,
    error::Error,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::Body,
    extract::{OriginalUri, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use percent_encoding::{percent_decode_str, percent_encode, utf8_percent_encode};
use serde::Deserialize;
use tokio::sync::Semaphore;
use url::Url;

const CACHE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_IN_FLIGHT: usize = 16;

#[derive(Clone)]
struct App {
    cache: cache::Cache,
    files: Arc<file_policy::FilePolicy>,
    permits: Arc<Semaphore>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchQuery {
    url: String,
}

type HttpError = (StatusCode, &'static str);

async fn fetch(
    State(app): State<App>,
    Query(query): Query<FetchQuery>,
) -> Result<StatusCode, HttpError> {
    let url = upstream::parse(&query.url)?;
    get_or_download(&app, url).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn serve(
    State(app): State<App>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let path = uri
        .path_and_query()
        .map(|value| value.as_str())
        .ok_or(upstream::INVALID_URL)?;
    let (target, download) = if let Some(target) = path.strip_prefix("/download/") {
        (target, true)
    } else {
        (
            path.strip_prefix("/serve/").ok_or(upstream::INVALID_URL)?,
            false,
        )
    };
    if target.starts_with(['/', '\\', '?']) {
        return Err(upstream::INVALID_URL);
    }

    let url = upstream::parse(&format!("https://{target}"))?;
    let mut cached = get_or_download(&app, url).await?;
    if download && let upstream::Upstream::Redirect(redirect) = &cached.entry {
        let target = upstream::parse(&redirect.location).map_err(|_| upstream::REDIRECT_DENIED)?;
        cached = get_or_download(&app, target).await?;
    }
    match cached.entry {
        upstream::Upstream::File(content_type, bytes) => {
            let mut response = if download {
                file_body(
                    HeaderValue::from_static("application/octet-stream"),
                    bytes,
                    &cached.original_name,
                )
            } else {
                file_response(
                    &headers,
                    content_type,
                    bytes,
                    &cached.original_name,
                    cached.expires_at,
                    cached.version_mismatch,
                )
            };
            let headers = response.headers_mut();
            headers.insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            headers.insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static("sandbox; default-src 'none'"),
            );
            Ok(response)
        }
        upstream::Upstream::Redirect(redirect) => {
            if download {
                return Err(upstream::REDIRECT_DENIED);
            }
            let status =
                StatusCode::from_u16(redirect.status).map_err(|_| upstream::REDIRECT_DENIED)?;
            let location =
                HeaderValue::from_str(&redirect.location).map_err(|_| upstream::REDIRECT_DENIED)?;
            let mut response = (status, [(header::LOCATION, location)]).into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            Ok(response)
        }
    }
}

fn file_body(content_type: HeaderValue, bytes: Vec<u8>, original_name: &str) -> Response {
    let mut response = Body::from(bytes).into_response();
    let headers = response.headers_mut();
    if content_type
        .as_bytes()
        .split(|byte| *byte == b';')
        .next()
        .is_some_and(|media_type| {
            media_type
                .trim_ascii()
                .eq_ignore_ascii_case(b"application/octet-stream")
        })
    {
        headers.insert(
            header::CONTENT_DISPOSITION,
            file_policy::attachment(original_name),
        );
    }
    headers.insert(header::CONTENT_TYPE, content_type);
    response
}

fn file_response(
    request: &HeaderMap,
    content_type: HeaderValue,
    bytes: Vec<u8>,
    original_name: &str,
    expires_at: u64,
    version_mismatch: bool,
) -> Response {
    // CRC32 plus byte length is a weak validator because collisions are possible.
    let tag = format!("\"{:08x}-{:x}\"", crc32fast::hash(&bytes), bytes.len());
    let unchanged = request.get_all(header::IF_NONE_MATCH).iter().any(|value| {
        let value = value.as_bytes().trim_ascii();
        value == b"*"
            || value.split(|byte| *byte == b',').any(|candidate| {
                let candidate = candidate.trim_ascii();
                candidate.strip_prefix(b"W/").unwrap_or(candidate) == tag.as_bytes()
            })
    });

    let mut response = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        file_body(content_type, bytes, original_name)
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let max_age = expires_at.saturating_sub(now).min(i32::MAX as u64);
    let cache_control = if version_mismatch {
        HeaderValue::from_static("no-cache")
    } else {
        HeaderValue::from_str(&format!("public, max-age={max_age}, must-revalidate"))
            .expect("numeric max-age is a valid header")
    };
    // An HTTP date uses a four-digit year.
    let date =
        httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(expires_at.min(253_402_300_799)));
    let headers = response.headers_mut();
    headers.insert(
        header::EXPIRES,
        HeaderValue::from_str(&date).expect("valid HTTP date"),
    );
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("W/{tag}")).expect("valid CRC ETag"),
    );
    headers.insert(header::CACHE_CONTROL, cache_control);
    response
}

fn cache_key(url: &Url) -> String {
    let authority = &url[url::Position::BeforeHost..url::Position::BeforePath];
    let mut key = String::new();
    if url.scheme() == "http" {
        key.push_str("http|");
    }
    key.extend(utf8_percent_encode(authority, file_policy::URL_COMPONENT));
    for segment in url.path().split(['/', '\\']).skip(1) {
        key.push(':');
        let decoded: Vec<_> = percent_decode_str(segment).collect();
        key.extend(percent_encode(&decoded, file_policy::URL_COMPONENT));
    }
    key
}

async fn get_or_download(app: &App, url: Url) -> Result<cache::CachedResponse, HttpError> {
    let _permit = app.permits.try_acquire().map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "too many requests in progress",
        )
    })?;
    let filename = app.files.filename(&url)?;
    app.cache
        .get_or_download(cache_key(&url), url, filename)
        .await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let settings = config::load()?;
    let connection_config = redis::AsyncConnectionConfig::new()
        .set_connection_timeout(CACHE_TIMEOUT)
        .set_response_timeout(CACHE_TIMEOUT);
    let cache = redis::Client::open(redis_url)?
        .get_multiplexed_async_connection_with_config(&connection_config)
        .await?;
    let storage = storage::Storage::new(settings.storage).await?;
    let app = App {
        cache: cache::Cache::new(cache, settings.cache, settings.origins, storage),
        files: Arc::new(settings.files),
        permits: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
    };
    let router = Router::new()
        .route("/api/v1/fetch", get(fetch))
        .route("/serve/{*target}", get(serve))
        .route("/download/{*target}", get(serve))
        .with_state(app);
    listeners::serve(settings.listeners, router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_cache_headers_and_conditional_requests() {
        let mut request = HeaderMap::new();
        let response = |headers: &HeaderMap, mismatch| {
            file_response(
                headers,
                HeaderValue::from_static("text/plain"),
                b"123456789".to_vec(),
                "test.txt",
                784_111_777,
                mismatch,
            )
        };
        let first = response(&request, false);
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(first.headers()[header::ETAG], "W/\"cbf43926-9\"");
        assert_eq!(
            first.headers()[header::EXPIRES],
            "Sun, 06 Nov 1994 08:49:37 GMT"
        );
        assert_eq!(
            first.headers()[header::CACHE_CONTROL],
            "public, max-age=0, must-revalidate"
        );

        request.append(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("W/\"other\""),
        );
        request.append(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"ignore\", \"cbf43926-9\""),
        );
        let conditional = response(&request, true);
        assert_eq!(conditional.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            conditional.headers()[header::ETAG],
            first.headers()[header::ETAG]
        );
        assert_eq!(
            conditional.headers()[header::EXPIRES],
            first.headers()[header::EXPIRES]
        );
        assert_eq!(conditional.headers()[header::CACHE_CONTROL], "no-cache");
        assert!(
            axum::body::to_bytes(conditional.into_body(), 0)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn canonical_cache_keys() {
        let key = |url| cache_key(&upstream::parse(url).unwrap());
        assert_eq!(
            key("https://Example.com/some\\static/image.jpg"),
            "example.com:some:static:image.jpg"
        );
        assert_eq!(key("https://example.com/a?t=123"), "example.com:a");
        assert_eq!(
            key("https://example.com/a?v=other#fragment"),
            key("https://example.com/a")
        );
        assert_ne!(key("https://example.com/a"), key("http://example.com/a"));
        assert_ne!(
            key("https://example.com/a:b"),
            key("https://example.com/a/b")
        );
        assert_eq!(
            key("https://example.com/café%20+%20notes.txt"),
            "example.com:caf%C3%A9%20%2B%20notes.txt"
        );
    }
}
