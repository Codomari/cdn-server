use axum::http::header;
use redis::aio::MultiplexedConnection;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::RequestError;

use super::{
    OriginPolicy,
    target::{MAX_URL_BYTES, valid_marker, validate},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct StoredRedirect {
    pub(crate) status: u16,
    pub(crate) location: String,
}

enum LocationFormat {
    Received,
    Cached,
}

pub(super) fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn redirect_target(
    origin: &Url,
    location: &str,
    format: LocationFormat,
) -> Result<Url, RequestError> {
    if location.is_empty()
        || location.len() > MAX_URL_BYTES
        || location
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || ch == '\\')
    {
        return Err(RequestError::RedirectDenied);
    }
    let bytes = location.as_bytes();
    if bytes.iter().enumerate().any(|(index, byte)| {
        *byte == b'%'
            && !bytes
                .get(index + 1..index + 3)
                .is_some_and(|pair| pair.iter().all(u8::is_ascii_hexdigit))
    }) {
        return Err(RequestError::RedirectDenied);
    }
    let target = match format {
        LocationFormat::Cached => Url::parse(location),
        LocationFormat::Received => origin.join(location),
    }
    .map_err(|_| RequestError::RedirectDenied)?;
    if origin.scheme() == "https" && target.scheme() != "https" {
        return Err(RequestError::RedirectDenied);
    }
    validate(&target).map_err(|_| RequestError::RedirectDenied)?;
    if target.query().is_some_and(|query| !valid_marker(query)) {
        return Err(RequestError::RedirectDenied);
    }
    Ok(target)
}

async fn approve_redirect(
    redis: &MultiplexedConnection,
    policy: &OriginPolicy,
    target: &Url,
) -> Result<(), RequestError> {
    policy
        .approve(redis, target)
        .await
        .map(|_| ())
        .map_err(|error| match error {
            RequestError::HostDenied | RequestError::IpDenied => RequestError::RedirectDenied,
            other => other,
        })
}

pub(crate) async fn validate_cached_redirect(
    redis: &MultiplexedConnection,
    policy: &OriginPolicy,
    origin: &Url,
    redirect: &StoredRedirect,
    enabled: bool,
) -> Result<(), RequestError> {
    if !enabled || !is_redirect(redirect.status) {
        return Err(RequestError::RedirectDenied);
    }
    let target = redirect_target(origin, &redirect.location, LocationFormat::Cached)?;
    if target.as_str() != redirect.location {
        return Err(RequestError::RedirectDenied);
    }
    approve_redirect(redis, policy, &target).await
}

pub(super) async fn read_redirect(
    redis: &MultiplexedConnection,
    policy: &OriginPolicy,
    origin: &Url,
    response: reqwest::Response,
    enabled: bool,
) -> Result<StoredRedirect, RequestError> {
    if !enabled {
        return Err(RequestError::RedirectDenied);
    }
    let mut headers = response.headers().get_all(header::LOCATION).iter();
    let location = headers
        .next()
        .and_then(|value| value.to_str().ok())
        .ok_or(RequestError::RedirectDenied)?;
    if headers.next().is_some() {
        return Err(RequestError::RedirectDenied);
    }
    let target = redirect_target(origin, location, LocationFormat::Received)?;
    approve_redirect(redis, policy, &target).await?;
    Ok(StoredRedirect {
        status: response.status().as_u16(),
        location: target.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_targets_are_absolute_and_keep_supported_markers() {
        let origin = Url::parse("https://example.com/css/main.css").unwrap();
        let location = "../main.css?v=2#section";
        let target = redirect_target(&origin, location, LocationFormat::Received).unwrap();
        assert_eq!(target.as_str(), "https://example.com/main.css?v=2#section");
        assert_eq!(
            redirect_target(&origin, target.as_str(), LocationFormat::Cached),
            Ok(target)
        );
        assert_eq!(
            redirect_target(&origin, location, LocationFormat::Cached),
            Err(RequestError::RedirectDenied)
        );

        for location in [
            "",
            "/main.css?other=2",
            "http://example.com/main.css",
            "/a%7",
        ] {
            assert_eq!(
                redirect_target(&origin, location, LocationFormat::Received),
                Err(RequestError::RedirectDenied)
            );
        }
    }
}
