use url::{Host, Url};

use crate::error::RequestError;

use super::origin::is_public;

const MAX_MARKER_BYTES: usize = 64;
pub(super) const MAX_URL_BYTES: usize = 8_192;

pub(crate) fn parse(input: &str) -> Result<Url, RequestError> {
    if input.len() > MAX_URL_BYTES || input.chars().any(char::is_control) {
        return Err(RequestError::InvalidUrl);
    }
    let mut url = Url::parse(input).map_err(|_| RequestError::InvalidUrl)?;
    validate(&url)?;
    if url.query().is_some_and(|query| !valid_marker(query)) {
        return Err(RequestError::InvalidVersionMarker);
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

pub(super) fn validate(url: &Url) -> Result<(), RequestError> {
    if url.as_str().len() > MAX_URL_BYTES
        || !matches!(url.scheme(), "https" | "http")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(RequestError::InvalidUrl);
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
    allowed.then_some(()).ok_or(RequestError::InvalidUrl)
}

#[cfg(test)]
mod tests {
    use super::*;

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
