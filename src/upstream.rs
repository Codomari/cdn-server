use std::{
    collections::HashSet,
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
use url::{Host, Url};

use crate::{HttpError, allowed_hosts::AllowedHosts, config::CacheConfig, storage::StoredRedirect};

const MAX_MARKER_BYTES: usize = 64;
const MAX_URL_BYTES: usize = 8_192;
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) const INVALID_URL: HttpError = (StatusCode::BAD_REQUEST, "invalid or disallowed URL");
const BAD_UPSTREAM: HttpError = (StatusCode::BAD_GATEWAY, "upstream download failed");
const TOO_LARGE: HttpError = (
    StatusCode::PAYLOAD_TOO_LARGE,
    "upstream file exceeds maxFilesize",
);
const TIMED_OUT: HttpError = (StatusCode::GATEWAY_TIMEOUT, "upstream download timed out");

const REFUSED: StatusCode = match StatusCode::from_u16(444) {
    Ok(status) => status,
    Err(_) => panic!("invalid HTTP status"),
};
const IP_DENIED: HttpError = (REFUSED, "upstream IP is not allowed");
pub(crate) const REDIRECT_DENIED: HttpError = (REFUSED, "upstream redirect is not allowed");

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

pub(crate) fn parse(input: &str) -> Result<Url, HttpError> {
    if input.len() > MAX_URL_BYTES || input.chars().any(char::is_control) {
        return Err(INVALID_URL);
    }
    let mut url = Url::parse(input).map_err(|_| INVALID_URL)?;
    validate(&url)?;
    if url.query().is_some_and(|query| !valid_marker(query)) {
        return Err((
            StatusCode::BAD_REQUEST,
            "expected a single t or v query marker",
        ));
    }
    url.set_fragment(None);
    Ok(url)
}

pub(crate) fn valid_marker(query: &str) -> bool {
    let Some((name, value)) = query.split_once('=') else {
        return false;
    };
    if value.is_empty() || value.len() > MAX_MARKER_BYTES {
        return false;
    }
    match name {
        "t" => value.bytes().all(|byte| byte.is_ascii_digit()),
        "v" => value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        _ => false,
    }
}

fn check_host(url: &Url, allowed_hosts: &AllowedHosts) -> Result<(), HttpError> {
    if url
        .host_str()
        .is_some_and(|host| allowed_hosts.allows(host))
    {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "upstream host is not allowed"))
    }
}

fn validate(url: &Url) -> Result<(), HttpError> {
    if url.as_str().len() > MAX_URL_BYTES
        || !matches!(url.scheme(), "https" | "http")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(INVALID_URL);
    }
    let allowed = match url.host() {
        Some(Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.');
            domain.contains('.')
                && !["localhost", "local", "internal", "home.arpa"]
                    .iter()
                    .any(|suffix| {
                        domain == *suffix
                            || domain
                                .strip_suffix(suffix)
                                .is_some_and(|name| name.ends_with('.'))
                    })
        }
        Some(Host::Ipv4(ip)) => is_public(ip.into()),
        Some(Host::Ipv6(ip)) => is_public(ip.into()),
        None => false,
    };
    allowed.then_some(()).ok_or(INVALID_URL)
}

struct NoDns;

impl Resolve for NoDns {
    fn resolve(&self, _: Name) -> Resolving {
        Box::pin(async {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "unmapped origin").into())
        })
    }
}

fn literal_ip(url: &Url) -> Option<IpAddr> {
    match url.host()? {
        Host::Ipv4(ip) => Some(ip.into()),
        Host::Ipv6(ip) => Some(ip.into()),
        Host::Domain(_) => None,
    }
}

pub(crate) struct OriginPolicy {
    pub(crate) allowed_hosts: AllowedHosts,
    pub(crate) whitelist: HashSet<IpAddr>,
    pub(crate) ttl_seconds: u64,
}

impl OriginPolicy {
    fn permits(&self, ip: IpAddr, literal: Option<IpAddr>) -> bool {
        is_public(ip) && literal.is_none_or(|value| value == ip && self.whitelist.contains(&ip))
    }

    pub(crate) async fn approve(
        &self,
        redis: &MultiplexedConnection,
        url: &Url,
    ) -> Result<IpAddr, HttpError> {
        check_host(url, &self.allowed_hosts)?;
        let host = url.host_str().ok_or(INVALID_URL)?;
        let hostname = host.trim_end_matches('.').to_ascii_lowercase();
        let key = format!("host:{hostname}:ip");
        let literal = literal_ip(url);
        let mut connection = redis.clone();
        let mut resolved = None;

        loop {
            let cached: Option<Vec<u8>> = redis::cmd("GET")
                .arg(&key)
                .query_async(&mut connection)
                .await
                .map_err(|error| {
                    if error.code() == Some("WRONGTYPE") {
                        IP_DENIED
                    } else {
                        crate::cache::cache_error(error)
                    }
                })?;
            if let Some(bytes) = cached {
                let ip = std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|value| value.parse::<IpAddr>().ok())
                    .filter(|ip| ip.to_string().as_bytes() == bytes && self.permits(*ip, literal));
                return ip.ok_or(IP_DENIED);
            }

            let ip = match (resolved, literal) {
                (Some(ip), _) | (_, Some(ip)) => ip,
                (None, None) => {
                    let absolute = format!("{hostname}.");
                    let addresses = tokio::time::timeout(
                        CONNECT_TIMEOUT,
                        tokio::net::lookup_host((absolute, 0)),
                    )
                    .await
                    .map_err(|_| IP_DENIED)?
                    .map_err(|_| IP_DENIED)?;
                    addresses
                        .map(|address| address.ip())
                        .min_by_key(|ip| {
                            (
                                !self.permits(*ip, literal),
                                !self.whitelist.contains(ip),
                                *ip,
                            )
                        })
                        .ok_or(IP_DENIED)?
                }
            };
            resolved = Some(ip);

            let stored: Option<String> = redis::cmd("SET")
                .arg(&key)
                .arg(ip.to_string())
                .arg("EX")
                .arg(self.ttl_seconds)
                .arg("NX")
                .query_async(&mut connection)
                .await
                .map_err(crate::cache::cache_error)?;
            if stored.is_some() {
                return self.permits(ip, literal).then_some(ip).ok_or(IP_DENIED);
            }
            // Another request populated this key. Check its value before using any address.
        }
    }
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => !matches!(
            ip.octets(),
            [0 | 10 | 127 | 224..=255, ..]
                | [100, 64..=127, ..]
                | [169, 254, ..]
                | [172, 16..=31, ..]
                | [192, 0, 0 | 2, _]
                | [192, 88, 99, _]
                | [192, 168, ..]
                | [198, 18 | 19, ..]
                | [198, 51, 100, _]
                | [203, 0, 113, _]
        ),
        IpAddr::V6(ip) => {
            let [first, second, ..] = ip.segments();
            // Limit to global unicast and exclude special-purpose subranges.
            (0x2000..=0x3fff).contains(&first)
                && !matches!(
                    (first, second),
                    (0x2001, 0..=0x01ff | 0x0db8)
                        | (0x2002, _)
                        | (0x3ffe, _)
                        | (0x3fff, 0..=0x0fff)
                )
        }
    }
}

fn request_error(error: reqwest::Error) -> HttpError {
    if error.is_timeout() {
        TIMED_OUT
    } else {
        BAD_UPSTREAM
    }
}

pub(crate) fn content_type(value: &[u8]) -> HeaderValue {
    HeaderValue::from_bytes(value)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn redirect_target(origin: &Url, location: &str, stored: bool) -> Result<Url, HttpError> {
    if location.is_empty()
        || location.len() > MAX_URL_BYTES
        || location
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || ch == '\\')
    {
        return Err(REDIRECT_DENIED);
    }
    let bytes = location.as_bytes();
    if bytes.iter().enumerate().any(|(index, byte)| {
        *byte == b'%'
            && !bytes
                .get(index + 1..index + 3)
                .is_some_and(|pair| pair.iter().all(u8::is_ascii_hexdigit))
    }) {
        return Err(REDIRECT_DENIED);
    }
    let target = if stored {
        Url::parse(location)
    } else {
        origin.join(location)
    }
    .map_err(|_| REDIRECT_DENIED)?;
    if origin.scheme() == "https" && target.scheme() != "https" {
        return Err(REDIRECT_DENIED);
    }
    validate(&target).map_err(|_| REDIRECT_DENIED)?;
    if target.query().is_some_and(|query| !valid_marker(query)) {
        return Err(REDIRECT_DENIED);
    }
    Ok(target)
}

async fn approve_redirect(
    redis: &MultiplexedConnection,
    policy: &OriginPolicy,
    target: &Url,
) -> Result<(), HttpError> {
    policy
        .approve(redis, target)
        .await
        .map(|_| ())
        .map_err(|error| {
            if error.0 == StatusCode::FORBIDDEN || error.0 == REFUSED {
                REDIRECT_DENIED
            } else {
                error
            }
        })
}

pub(crate) async fn validate_cached_redirect(
    redis: &MultiplexedConnection,
    policy: &OriginPolicy,
    origin: &Url,
    redirect: &StoredRedirect,
    enabled: bool,
) -> Result<(), HttpError> {
    if !enabled || !is_redirect(redirect.status) {
        return Err(REDIRECT_DENIED);
    }
    let target = redirect_target(origin, &redirect.location, true)?;
    if target.as_str() != redirect.location {
        return Err(REDIRECT_DENIED);
    }
    approve_redirect(redis, policy, &target).await
}

pub(crate) async fn download(
    redis: &MultiplexedConnection,
    policy: &OriginPolicy,
    url: Url,
    ip: IpAddr,
    config: CacheConfig,
) -> Result<Upstream, HttpError> {
    tokio::time::timeout(
        FETCH_TIMEOUT,
        download_inner(redis, policy, url, ip, config),
    )
    .await
    .map_err(|_| TIMED_OUT)?
}

async fn download_inner(
    redis: &MultiplexedConnection,
    policy: &OriginPolicy,
    url: Url,
    ip: IpAddr,
    config: CacheConfig,
) -> Result<Upstream, HttpError> {
    let client = client(url.host_str().ok_or(INVALID_URL)?, ip).map_err(request_error)?;
    let mut response = client
        .get(url.clone())
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(request_error)?;

    let status = response.status().as_u16();
    if is_redirect(status) {
        if !config.store_redirects {
            return Err(REDIRECT_DENIED);
        }
        let mut headers = response.headers().get_all(header::LOCATION).iter();
        let location = headers
            .next()
            .and_then(|value| value.to_str().ok())
            .ok_or(REDIRECT_DENIED)?;
        if headers.next().is_some() {
            return Err(REDIRECT_DENIED);
        }
        let target = redirect_target(&url, location, false)?;
        approve_redirect(redis, policy, &target).await?;
        return Ok(Upstream::Redirect(StoredRedirect {
            status,
            location: target.into(),
        }));
    }
    if response.status() == StatusCode::NOT_FOUND {
        return Err((StatusCode::NOT_FOUND, "upstream file not found"));
    }
    if response.status() != StatusCode::OK {
        return Err(BAD_UPSTREAM);
    }
    if response
        .headers()
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return Err(BAD_UPSTREAM);
    }
    if response
        .content_length()
        .is_some_and(|size| size > config.max_file_size as u64)
    {
        return Err(TOO_LARGE);
    }
    let media_type = response.headers().get(header::CONTENT_TYPE).map_or_else(
        || content_type(b"application/octet-stream"),
        |value| value.clone(),
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if chunk.len() > config.max_file_size - bytes.len() {
            return Err(TOO_LARGE);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Upstream::File(media_type, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_dns_hosts_can_use_public_ips_outside_the_ip_whitelist() {
        let listed = "1.1.1.1".parse().unwrap();
        let unlisted = "8.8.8.8".parse().unwrap();
        let private = "192.168.1.1".parse().unwrap();
        let policy = OriginPolicy {
            allowed_hosts: AllowedHosts::new(vec!["example.com".into()]).unwrap(),
            whitelist: HashSet::from([listed, private]),
            ttl_seconds: 3600,
        };
        assert!(policy.permits(unlisted, None));
        assert!(!policy.permits(private, None));
        assert!(policy.permits(listed, Some(listed)));
        assert!(!policy.permits(listed, Some(unlisted)));
        assert!(!policy.permits(unlisted, Some(unlisted)));
    }

    #[test]
    fn redirect_targets_are_absolute_and_keep_supported_markers() {
        let origin = Url::parse("https://example.com/css/main.css").unwrap();
        let location = "../main.css?v=2#section";
        let target = redirect_target(&origin, location, false).unwrap();
        assert_eq!(target.as_str(), "https://example.com/main.css?v=2#section");
        assert_eq!(redirect_target(&origin, target.as_str(), true), Ok(target));
        assert_eq!(
            redirect_target(&origin, location, true),
            Err(REDIRECT_DENIED)
        );

        for location in [
            "",
            "/main.css?other=2",
            "http://example.com/main.css",
            "/a%7",
        ] {
            assert_eq!(
                redirect_target(&origin, location, false),
                Err(REDIRECT_DENIED)
            );
        }
    }

    #[test]
    fn inbound_query_accepts_one_bounded_marker() {
        for query in ["t=123", "v=1.2.3-beta_X", "v=A0"] {
            let url = parse(&format!("https://something.com/image?{query}")).unwrap();
            assert_eq!(url.query(), Some(query));
        }
        for query in [
            "", "t=", "t=-1", "t=１", "v=", "v=a+b", "v=a%2Eb", "v=a&v=b", "t=1&v=b", "x=foo",
        ] {
            assert!(parse(&format!("https://something.com/image?{query}")).is_err());
        }
        assert!(!valid_marker(&format!("v={}", "a".repeat(65))));
    }
}
