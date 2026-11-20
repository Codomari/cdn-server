use std::{
    collections::HashSet,
    error::Error,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
};

use serde::Deserialize;

const UNSPECIFIED_IPV4: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct HttpConfig {
    enabled: bool,
    bind: IpAddr,
    port: u16,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: UNSPECIFIED_IPV4,
            port: 80,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct HttpsConfig {
    enabled: bool,
    bind: IpAddr,
    port: u16,
    tls: Option<TlsConfig>,
}

impl Default for HttpsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: UNSPECIFIED_IPV4,
            port: 443,
            tls: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TlsConfig {
    pub(crate) hosts: Vec<TlsHost>,
    #[serde(default = "certbot_state_dir")]
    pub(crate) state_dir: PathBuf,
    #[serde(default)]
    pub(crate) staging: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase", deny_unknown_fields)]
pub(crate) enum TlsHost {
    File {
        name: String,
        key: PathBuf,
        cert: PathBuf,
    },
    Certbot {
        name: String,
        account: String,
    },
}

fn certbot_state_dir() -> PathBuf {
    "certbot/".into()
}

pub(crate) struct Listeners {
    pub(crate) http: Option<SocketAddr>,
    pub(crate) https: Option<(SocketAddr, TlsConfig)>,
}

impl Listeners {
    pub(super) fn from_config(
        http: &HttpConfig,
        https: &mut HttpsConfig,
    ) -> Result<Self, Box<dyn Error>> {
        let http = http.enabled.then(|| SocketAddr::new(http.bind, http.port));
        if !https.enabled {
            return Ok(Self {
                http: Some(http.ok_or("enable at least one HTTP or HTTPS listener")?),
                https: None,
            });
        }

        let mut tls = https
            .tls
            .take()
            .ok_or("https.tls is required when HTTPS is enabled")?;
        tls.validate(http.is_some())?;

        Ok(Self {
            http,
            https: Some((SocketAddr::new(https.bind, https.port), tls)),
        })
    }
}

impl TlsConfig {
    fn validate(&mut self, http_enabled: bool) -> Result<(), Box<dyn Error>> {
        if self.hosts.is_empty() {
            return Err("https.tls.hosts must contain at least one host".into());
        }

        let mut names = HashSet::new();
        for host in &mut self.hosts {
            let name = match host {
                TlsHost::File { name, key, cert } => {
                    if key.as_os_str().is_empty() || cert.as_os_str().is_empty() {
                        return Err(
                            format!("TLS host {name} requires key and cert PEM paths").into()
                        );
                    }
                    name
                }
                TlsHost::Certbot { name, account } => {
                    if !http_enabled {
                        return Err("Certbot HTTP-01 requires the HTTP listener".into());
                    }
                    if self.state_dir.as_os_str().is_empty() {
                        return Err("https.tls.stateDir must be a directory path".into());
                    }
                    if !account.split_once('@').is_some_and(|(local, domain)| {
                        !local.is_empty()
                            && !local.chars().any(char::is_whitespace)
                            && dns_name(domain)
                    }) {
                        return Err(format!(
                            "Certbot host {name} requires an account email address"
                        )
                        .into());
                    }
                    name
                }
            };
            if !dns_name(name) {
                return Err(format!(
                    "TLS host {name} must be an explicit ASCII or Punycode DNS name"
                )
                .into());
            }
            name.make_ascii_lowercase();
            if !names.insert(name.as_str()) {
                return Err(format!("duplicate TLS host {name}").into());
            }
        }
        Ok(())
    }
}

fn dns_name(name: &str) -> bool {
    name.len() <= 253
        && name.contains('.')
        && name.parse::<IpAddr>().is_err()
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, files::FilePolicy};

    #[test]
    fn listener_defaults_and_shipped_certbot_configuration() {
        let mut defaults = Config::default();
        assert_eq!(defaults.https.bind, UNSPECIFIED_IPV4);
        let listeners = Listeners::from_config(&defaults.http, &mut defaults.https).unwrap();
        assert_eq!(listeners.http, Some("0.0.0.0:80".parse().unwrap()));
        assert!(listeners.https.is_none());

        let mut shipped: Config =
            serde_yaml::from_str(include_str!("../../config.example.yaml")).unwrap();
        FilePolicy::new(
            std::mem::take(&mut shipped.allowed_extensions),
            std::mem::take(&mut shipped.denied_extensions),
        )
        .unwrap();
        assert_eq!(shipped.http.bind, UNSPECIFIED_IPV4);
        assert_eq!(shipped.https.bind, UNSPECIFIED_IPV4);
        let tls = shipped.https.tls.as_ref().unwrap();
        assert!(matches!(&tls.hosts[..], [
            TlsHost::Certbot { name, account },
            TlsHost::File { name: file_name, .. },
        ] if name == "cdn.example.com" && account == "ops@example.com"
            && file_name == "cdn-files.example.com"));
        assert!(
            Listeners::from_config(&shipped.http, &mut shipped.https)
                .unwrap()
                .https
                .is_none()
        );
        shipped.https.enabled = true;
        assert_eq!(
            Listeners::from_config(&shipped.http, &mut shipped.https)
                .unwrap()
                .https
                .unwrap()
                .0
                .port(),
            443
        );
    }

    #[test]
    fn validates_enabled_tls_hosts() {
        let parse = |yaml: &str| serde_yaml::from_str::<Config>(yaml).unwrap();
        let mut disabled = parse("https: {tls: {hosts: []}}");
        assert!(Listeners::from_config(&disabled.http, &mut disabled.https).is_ok());

        for hosts in [
            "[]",
            "[{name: '*.example.com', provider: file, key: key.pem, cert: cert.pem}]",
            "[{name: a.example.com, provider: certbot, account: invalid}]",
            "[{name: A.example.com, provider: file, key: key.pem, cert: cert.pem}, {name: a.example.com, provider: certbot, account: certs@example.com}]",
        ] {
            let mut config = parse(&format!(
                "https: {{enabled: true, tls: {{hosts: {hosts}}}}}"
            ));
            assert!(
                Listeners::from_config(&config.http, &mut config.https).is_err(),
                "{hosts}"
            );
        }

        let mut config = parse(
            "http: {enabled: false}\nhttps: {enabled: true, tls: {hosts: [{name: a.example.com, provider: certbot, account: contact@example.com}]}}",
        );
        assert!(Listeners::from_config(&config.http, &mut config.https).is_err());

        let mut config = parse(
            "http: {enabled: false}\nhttps: {enabled: true, tls: {hosts: [{name: A.example.com, provider: file, key: key.pem, cert: cert.pem}]}}",
        );
        let listeners = Listeners::from_config(&config.http, &mut config.https).unwrap();
        assert!(listeners.http.is_none());
        assert!(
            matches!(&listeners.https.unwrap().1.hosts[..], [TlsHost::File { name, .. }] if name == "a.example.com")
        );
        assert!(
            serde_yaml::from_str::<Config>(
                "https: {tls: {hosts: [{name: a.example.com, provider: file, cert: cert.pem}]}}"
            )
            .is_err()
        );
    }
}
