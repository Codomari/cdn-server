mod allowed_hosts;
mod origin;
mod redirect;
mod target;

use std::{
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use axum::http::{HeaderValue, StatusCode, header};
use redis::aio::MultiplexedConnection;
use reqwest::{
    Client,
    dns::{Name, Resolve, Resolving},
    redirect::Policy,
};
use url::Url;

use crate::{config::CacheConfig, error::RequestError};

pub(crate) use allowed_hosts::AllowedHosts;
pub(crate) use origin::OriginPolicy;
pub(crate) use redirect::{StoredRedirect, validate_cached_redirect};
pub(crate) use target::{parse, valid_marker};

const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) enum Upstream {
    File(HeaderValue, Vec<u8>),
    Redirect(StoredRedirect),
}

fn client(host: &str, ip: IpAddr) -> Result<Client, reqwest::Error> {
    Client::builder()
        .no_proxy()
        .user_agent(concat!("cdn-server/", env!("CARGO_PKG_VERSION")))
        .redirect(Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_max_idle_per_host(0)
        .dns_resolver(Arc::new(NoDns))
        .resolve(host, SocketAddr::new(ip, 0))
        .build()
}

struct NoDns;

impl Resolve for NoDns {
    fn resolve(&self, _: Name) -> Resolving {
        Box::pin(async {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "unmapped origin").into())
        })
    }
}

fn request_error(error: reqwest::Error) -> RequestError {
    if error.is_timeout() {
        RequestError::UpstreamTimeout
    } else {
        RequestError::UpstreamFailure
    }
}

fn check_file_status(status: StatusCode) -> Result<(), RequestError> {
    match status {
        StatusCode::OK => Ok(()),
        StatusCode::FORBIDDEN => Err(RequestError::UpstreamDenied),
        StatusCode::NOT_FOUND => Err(RequestError::UpstreamNotFound),
        _ => Err(RequestError::UpstreamFailure),
    }
}

pub(crate) fn content_type(value: &[u8]) -> HeaderValue {
    HeaderValue::from_bytes(value)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}

pub(crate) async fn download(
    redis: &MultiplexedConnection,
    policy: &OriginPolicy,
    url: Url,
    ip: IpAddr,
    config: CacheConfig,
) -> Result<Upstream, RequestError> {
    tokio::time::timeout(
        FETCH_TIMEOUT,
        download_inner(redis, policy, url, ip, config),
    )
    .await
    .map_err(|_| RequestError::UpstreamTimeout)?
}

async fn download_inner(
    redis: &MultiplexedConnection,
    policy: &OriginPolicy,
    url: Url,
    ip: IpAddr,
    config: CacheConfig,
) -> Result<Upstream, RequestError> {
    let client =
        client(url.host_str().ok_or(RequestError::InvalidUrl)?, ip).map_err(request_error)?;
    let mut response = client
        .get(url.clone())
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(request_error)?;

    let status = response.status().as_u16();
    if redirect::is_redirect(status) {
        let redirect =
            redirect::read_redirect(redis, policy, &url, response, config.store_redirects).await?;
        return Ok(Upstream::Redirect(redirect));
    }
    check_file_status(response.status())?;
    if response
        .headers()
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return Err(RequestError::UpstreamFailure);
    }
    if response
        .content_length()
        .is_some_and(|size| size > config.max_file_size as u64)
    {
        return Err(RequestError::FileTooLarge);
    }
    let media_type = response.headers().get(header::CONTENT_TYPE).map_or_else(
        || content_type(b"application/octet-stream"),
        |value| value.clone(),
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if chunk.len() > config.max_file_size - bytes.len() {
            return Err(RequestError::FileTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Upstream::File(media_type, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_file_status_preserves_forbidden_and_not_found() {
        assert_eq!(check_file_status(StatusCode::OK), Ok(()));
        assert_eq!(
            check_file_status(StatusCode::FORBIDDEN),
            Err(RequestError::UpstreamDenied)
        );
        assert_eq!(
            check_file_status(StatusCode::NOT_FOUND),
            Err(RequestError::UpstreamNotFound)
        );
        assert_eq!(
            check_file_status(StatusCode::INTERNAL_SERVER_ERROR),
            Err(RequestError::UpstreamFailure)
        );
    }
}
