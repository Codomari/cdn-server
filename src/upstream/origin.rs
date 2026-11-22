use std::{collections::HashSet, net::IpAddr};

use redis::aio::MultiplexedConnection;
use url::{Host, Url};

use crate::error::RequestError;

use super::{AllowedHosts, CONNECT_TIMEOUT};

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
    fn check_ip(
        &self,
        ip: IpAddr,
        literal: Option<IpAddr>,
        host_allowed: bool,
    ) -> Result<IpAddr, RequestError> {
        if !is_public(ip) || literal.is_some_and(|literal| literal != ip) {
            return Err(RequestError::IpDenied);
        }
        if (literal.is_some() || !host_allowed) && !self.whitelist.contains(&ip) {
            return Err(if host_allowed {
                RequestError::IpDenied
            } else {
                RequestError::HostDenied
            });
        }
        Ok(ip)
    }

    fn select_dns_ip(
        &self,
        addresses: impl Iterator<Item = IpAddr>,
        host_allowed: bool,
    ) -> Result<IpAddr, RequestError> {
        let ip = addresses
            .filter(|ip| is_public(*ip))
            .min_by_key(|ip| (!self.whitelist.contains(ip), *ip))
            .ok_or(RequestError::IpDenied)?;
        self.check_ip(ip, None, host_allowed)
    }

    pub(crate) async fn approve(
        &self,
        redis: &MultiplexedConnection,
        url: &Url,
    ) -> Result<IpAddr, RequestError> {
        let host = url.host_str().ok_or(RequestError::InvalidUrl)?;
        let host_allowed = self
            .allowed_hosts
            .matches(host)
            .ok_or(RequestError::HostDenied)?;
        let literal = literal_ip(url);
        if !host_allowed && (literal.is_some() || self.whitelist.is_empty()) {
            return Err(RequestError::HostDenied);
        }
        let hostname = host.trim_end_matches('.').to_ascii_lowercase();
        let key = format!("host:{hostname}:ip");
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
                    .filter(|ip| ip.to_string().as_bytes() == bytes)
                    .ok_or(RequestError::IpDenied)?;
                return self.check_ip(ip, literal, host_allowed);
            }

            let ip = match (resolved, literal) {
                (Some(ip), _) => ip,
                (None, Some(ip)) => self.check_ip(ip, literal, host_allowed)?,
                (None, None) => {
                    let absolute = format!("{hostname}.");
                    let addresses = tokio::time::timeout(
                        CONNECT_TIMEOUT,
                        tokio::net::lookup_host((absolute, 0)),
                    )
                    .await
                    .map_err(|_| RequestError::IpDenied)?
                    .map_err(|_| RequestError::IpDenied)?;
                    self.select_dns_ip(addresses.map(|address| address.ip()), host_allowed)?
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
                return Ok(ip);
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
        assert_eq!(policy.check_ip(unlisted, None, true), Ok(unlisted));
        assert_eq!(
            policy.check_ip(private, None, true),
            Err(RequestError::IpDenied)
        );
        assert_eq!(policy.check_ip(listed, Some(listed), true), Ok(listed));
        assert_eq!(
            policy.check_ip(listed, Some(unlisted), true),
            Err(RequestError::IpDenied)
        );
        assert_eq!(
            policy.check_ip(unlisted, Some(unlisted), true),
            Err(RequestError::IpDenied)
        );
    }

    #[test]
    fn unlisted_dns_hosts_require_a_whitelisted_public_ip() {
        let listed = "49.13.25.133".parse().unwrap();
        let unlisted = "8.8.8.8".parse().unwrap();
        let private = "192.168.1.1".parse().unwrap();
        let policy = OriginPolicy {
            allowed_hosts: AllowedHosts::new(vec![]).unwrap(),
            whitelist: HashSet::from([listed, private]),
            ttl_seconds: 3600,
        };

        assert_eq!(
            policy.select_dns_ip([unlisted, private, listed].into_iter(), false),
            Ok(listed)
        );
        assert_eq!(
            policy.select_dns_ip([unlisted, private].into_iter(), false),
            Err(RequestError::HostDenied)
        );
        assert_eq!(
            policy.select_dns_ip([unlisted].into_iter(), true),
            Ok(unlisted)
        );
        assert_eq!(policy.check_ip(listed, None, false), Ok(listed));
        assert_eq!(
            policy.check_ip(unlisted, None, false),
            Err(RequestError::HostDenied)
        );
        assert_eq!(
            policy.check_ip(private, None, false),
            Err(RequestError::IpDenied)
        );
    }
}
