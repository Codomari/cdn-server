use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::error::RequestError;

const REFUSED: StatusCode = match StatusCode::from_u16(444) {
    Ok(status) => status,
    Err(_) => panic!("invalid HTTP status"),
};

impl IntoResponse for RequestError {
    fn into_response(self) -> Response {
        use RequestError::*;

        let (status, message) = match self {
            InvalidUrl => (StatusCode::BAD_REQUEST, "invalid or disallowed URL"),
            InvalidVersionMarker => (
                StatusCode::BAD_REQUEST,
                "expected a single t or v query marker",
            ),
            InvalidFilename => (StatusCode::BAD_REQUEST, "invalid filename"),
            ExtensionDenied => (StatusCode::FORBIDDEN, "file extension is not permitted"),
            HostDenied => (StatusCode::FORBIDDEN, "upstream host is not allowed"),
            IpDenied => (REFUSED, "upstream IP is not allowed"),
            RedirectDenied => (REFUSED, "upstream redirect is not allowed"),
            UpstreamNotFound => (StatusCode::NOT_FOUND, "upstream file not found"),
            UpstreamFailure => (StatusCode::BAD_GATEWAY, "upstream download failed"),
            FileTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "upstream file exceeds maxFilesize",
            ),
            UpstreamTimeout => (StatusCode::GATEWAY_TIMEOUT, "upstream download timed out"),
            CacheUnavailable => (StatusCode::SERVICE_UNAVAILABLE, "cache unavailable"),
            StorageUnavailable => (StatusCode::SERVICE_UNAVAILABLE, "file storage unavailable"),
            UnsupportedCacheKeyType => (
                StatusCode::SERVICE_UNAVAILABLE,
                "cache key has an unsupported type",
            ),
            TooManyRequests => (
                StatusCode::SERVICE_UNAVAILABLE,
                "too many requests in progress",
            ),
        };
        (status, message).into_response()
    }
}
