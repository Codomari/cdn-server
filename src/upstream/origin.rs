use std::{collections::HashSet, net::IpAddr};

use redis::aio::MultiplexedConnection;
use url::{Host, Url};

use crate::error::RequestError;

use super::{AllowedHosts, CONNECT_TIMEOUT};

fn check_host(url: &Url, allowed_hosts: &AllowedHosts) -> Result<(), RequestError> {
    if url
        .host_str()
        .is_some_and(|host| allowed_hosts.allows(host))
    {
        Ok(())
    } else {
        Err(RequestError::HostDenied)
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
    ) -> Result<IpAddr, RequestError> {
        check_host(url, &self.allowed_hosts)?;
        let host = url.host_str().ok_or(RequestError::InvalidUrl)?;
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
                        RequestError::IpDenied
                    } else {
                        RequestError::CacheUnavailable
                    }
                })?;
            if let Some(bytes) = cached {
                let ip = std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|value| value.parse::<IpAddr>().ok())
                    .filter(|ip| ip.to_string().as_bytes() == bytes && self.permits(*ip, literal));
                return ip.ok_or(RequestError::IpDenied);
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
                    .map_err(|_| RequestError::IpDenied)?
                    .map_err(|_| RequestError::IpDenied)?;
                    addresses
                        .map(|address| address.ip())
                        .min_by_key(|ip| {
                            (
                                !self.permits(*ip, literal),
                                !self.whitelist.contains(ip),
                                *ip,
                            )
                        })
                        .ok_or(RequestError::IpDenied)?
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
                .map_err(|_| RequestError::CacheUnavailable)?;
            if stored.is_some() {
                return self
                    .permits(ip, literal)
                    .then_some(ip)
                    .ok_or(RequestError::IpDenied);
            }
            // Another request populated this key. Check its value before using any address.
        }
    }
}

pub(super) fn is_public(ip: IpAddr) -> bool {
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
}
