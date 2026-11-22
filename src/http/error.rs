use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::error::RequestError;

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
            IpDenied => (StatusCode::FORBIDDEN, "upstream IP is not allowed"),
            OriginAddressNotFound => (StatusCode::NOT_FOUND, "upstream address not found"),
            RedirectDenied => (StatusCode::FORBIDDEN, "upstream redirect is not allowed"),
            RedirectTargetNotFound => (StatusCode::NOT_FOUND, "upstream redirect target not found"),
            UpstreamDenied => (StatusCode::FORBIDDEN, "upstream denied access"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_denials_and_missing_destinations_use_standard_statuses() {
        for (error, expected) in [
            (RequestError::HostDenied, StatusCode::FORBIDDEN),
            (RequestError::IpDenied, StatusCode::FORBIDDEN),
            (RequestError::RedirectDenied, StatusCode::FORBIDDEN),
            (RequestError::UpstreamDenied, StatusCode::FORBIDDEN),
            (RequestError::OriginAddressNotFound, StatusCode::NOT_FOUND),
            (RequestError::RedirectTargetNotFound, StatusCode::NOT_FOUND),
            (RequestError::UpstreamNotFound, StatusCode::NOT_FOUND),
        ] {
            assert_eq!(error.into_response().status(), expected);
        }
    }
}
