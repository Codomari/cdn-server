use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

use crate::files;

pub(super) fn download_file(bytes: Vec<u8>, original_name: &str) -> Response {
    let mut response = file_body(
        HeaderValue::from_static("application/octet-stream"),
        bytes,
        original_name,
    );
    file_security_headers(&mut response);
    response
}

fn file_security_headers(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
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
            files::attachment(original_name),
        );
    }
    headers.insert(header::CONTENT_TYPE, content_type);
    response
}

pub(super) fn serve_file(
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
    file_security_headers(&mut response);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_cache_headers_and_conditional_requests() {
        let mut request = HeaderMap::new();
        let response = |headers: &HeaderMap, mismatch| {
            serve_file(
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
}
