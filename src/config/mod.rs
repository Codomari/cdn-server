mod cors;
mod listeners;

use std::{
    collections::HashSet, env, error::Error, fs, io, net::IpAddr, path::PathBuf, time::Duration,
};

use serde::Deserialize;

use crate::{
    files::FilePolicy,
    upstream::{AllowedHosts, OriginPolicy},
};

pub(crate) use cors::CorsConfig;
use listeners::{HttpConfig, HttpsConfig};
pub(crate) use listeners::{Listeners, TlsHost};

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    http: HttpConfig,
    https: HttpsConfig,
    cors: CorsConfig,
    freshness: String,
    stale_retention: String,
    #[serde(rename = "allowedHosts")]
    allowed_hosts: Vec<String>,
    #[serde(rename = "hostIPWhitelist")]
    host_ip_whitelist: HashSet<IpAddr>,
    #[serde(rename = "hostIPCacheTtl")]
    host_ip_cache_ttl: String,
    #[serde(rename = "storeRedirects")]
    store_redirects: bool,
    storage: PathBuf,
    #[serde(rename = "allowedExtensions")]
    allowed_extensions: Vec<String>,
    #[serde(rename = "deniedExtensions")]
    denied_extensions: Vec<String>,
    #[serde(rename = "maxFilesize")]
    max_filesize: FileSize,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum FileSize {
    Bytes(usize),
    WithUnit(String),
}

impl Default for Config {
    fn default() -> Self {
        Self {
            http: HttpConfig::default(),
            https: HttpsConfig::default(),
            cors: CorsConfig::default(),
            freshness: "24h".into(),
            stale_retention: "7d".into(),
            allowed_hosts: Vec::new(),
            host_ip_whitelist: HashSet::new(),
            host_ip_cache_ttl: "1d".into(),
            store_redirects: false,
            storage: "/etc/cdn-server/storage".into(),
            allowed_extensions: Vec::new(),
            denied_extensions: Vec::new(),
            max_filesize: FileSize::Bytes(8 * 1024 * 1024),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CacheConfig {
    pub(crate) freshness: u64,
    pub(crate) stale_retention: u64,
    pub(crate) max_file_size: usize,
    pub(crate) store_redirects: bool,
}

pub(crate) struct Settings {
    pub(crate) listeners: Listeners,
    pub(crate) cors: CorsConfig,
    pub(crate) cache: CacheConfig,
    pub(crate) origins: OriginPolicy,
    pub(crate) storage: PathBuf,
    pub(crate) files: FilePolicy,
}

pub(crate) fn load() -> Result<Settings, Box<dyn Error>> {
    read_config()?.try_into()
}

fn read_config() -> Result<Config, Box<dyn Error>> {
    let explicit = env::var_os("CONFIG_PATH");
    let path = explicit.clone().unwrap_or_else(|| "config.yaml".into());
    match fs::read_to_string(&path) {
        Ok(contents) => Ok(serde_yaml::from_str(&contents)?),
        Err(error) if explicit.is_none() && error.kind() == io::ErrorKind::NotFound => {
            Ok(Config::default())
        }
        Err(error) => Err(error.into()),
    }
}

impl TryFrom<Config> for Settings {
    type Error = Box<dyn Error>;

    fn try_from(mut config: Config) -> Result<Self, Self::Error> {
        if config.storage.as_os_str().is_empty() {
            return Err("storage must be a directory path".into());
        }
        let allowed_hosts = AllowedHosts::new(config.allowed_hosts)?;
        let ttl = humantime::parse_duration(&config.host_ip_cache_ttl)
            .map_err(|error| format!("invalid hostIPCacheTtl: {error}"))?;
        if ttl.as_secs() == 0 || ttl.subsec_nanos() != 0 || ttl.as_secs() > i64::MAX as u64 / 2000 {
            return Err(
                "hostIPCacheTtl must be a positive whole-second duration in the supported range"
                    .into(),
            );
        }
        let origins = OriginPolicy {
            allowed_hosts,
            whitelist: config.host_ip_whitelist,
            ttl_seconds: ttl.as_secs(),
        };
        let files = FilePolicy::new(config.allowed_extensions, config.denied_extensions)?;
        Ok(Self {
            listeners: Listeners::from_config(&config.http, &mut config.https)?,
            cors: config.cors,
            cache: CacheConfig::from_config(
                &config.freshness,
                &config.stale_retention,
                config.max_filesize,
                config.store_redirects,
            )?,
            origins,
            storage: config.storage,
            files,
        })
    }
}

impl FileSize {
    fn bytes(self) -> Result<usize, Box<dyn Error>> {
        let size = match self {
            Self::Bytes(size) => Some(size),
            Self::WithUnit(value) => {
                let value = value.trim();
                let boundary = value
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(value.len());
                let (number, suffix) = value.split_at(boundary);
                let unit = match suffix.trim().to_ascii_lowercase().as_str() {
                    "" | "b" => 1,
                    "kb" => 1_000,
                    "mb" => 1_000_000,
                    "gb" => 1_000_000_000,
                    "kib" => 1 << 10,
                    "mib" => 1 << 20,
                    "gib" => 1 << 30,
                    _ => return Err("unknown maxFilesize unit".into()),
                };
                number.parse::<usize>()?.checked_mul(unit)
            }
        };
        size.filter(|size| *size > 0)
            .ok_or_else(|| "maxFilesize must be a positive supported byte count".into())
    }
}

impl CacheConfig {
    fn from_config(
        freshness: &str,
        stale_retention: &str,
        max_filesize: FileSize,
        store_redirects: bool,
    ) -> Result<Self, Box<dyn Error>> {
        let freshness = humantime::parse_duration(freshness)?;
        let stale_retention = humantime::parse_duration(stale_retention)?;
        if freshness < Duration::from_secs(3600) {
            return Err("freshness must be at least 1h".into());
        }
        if freshness.subsec_nanos() != 0 || stale_retention.subsec_nanos() != 0 {
            return Err("cache durations must use whole seconds".into());
        }
        // Redis stores absolute expiration internally in signed milliseconds.
        let total = freshness
            .checked_add(stale_retention)
            .ok_or("cache durations exceed the supported range")?;
        if total.as_secs() > (i64::MAX as u64 / 2000) {
            return Err("cache durations exceed the supported range".into());
        }
        Ok(Self {
            freshness: freshness.as_secs(),
            stale_retention: stale_retention.as_secs(),
            max_file_size: max_filesize.bytes()?,
            store_redirects,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cors_defaults_to_permissive_and_uses_boolean_fields() {
        for yaml in ["{}", "cors: {}", include_str!("../../config.example.yaml")] {
            let config: Config = serde_yaml::from_str(yaml).unwrap();
            assert!(config.cors.enable);
            assert!(config.cors.allow.origin);
            assert!(config.cors.allow.methods);
            assert!(config.cors.allow.headers);
            assert!(config.cors.allow.expose_headers);
        }

        let partial: Config = serde_yaml::from_str("cors: { allow: { origin: false } }").unwrap();
        assert!(partial.cors.enable);
        assert!(!partial.cors.allow.origin);
        assert!(partial.cors.allow.methods);
        assert!(partial.cors.allow.headers);
        assert!(partial.cors.allow.expose_headers);
        assert!(serde_yaml::from_str::<Config>("cors: { allow: { origin: '*' } }").is_err());
        assert!(serde_yaml::from_str::<Config>("cors: { allow: { method: true } }").is_err());
    }

    #[test]
    fn host_patterns_default_to_empty_and_use_yaml_array() {
        let empty = serde_yaml::from_str::<Config>("{}").unwrap();
        assert!(empty.allowed_hosts.is_empty());
        let examples =
            serde_yaml::from_str::<Config>(include_str!("../../config.example.yaml")).unwrap();
        assert_eq!(
            examples.allowed_hosts,
            [
                "media.example.com",
                "*.assets.example.net",
                "static.*.example.org"
            ]
        );
        assert!(serde_yaml::from_str::<Config>("allowedHosts: something.com").is_err());
    }

    #[test]
    fn origin_ip_whitelist_uses_typed_yaml_addresses() {
        let defaults: Config = serde_yaml::from_str("{}").unwrap();
        assert!(defaults.host_ip_whitelist.is_empty());
        assert_eq!(
            humantime::parse_duration(&defaults.host_ip_cache_ttl).unwrap(),
            Duration::from_secs(86_400)
        );

        let configured: Config = serde_yaml::from_str(
            "hostIPWhitelist: [1.1.1.1, '2606:4700:4700::1111']\nhostIPCacheTtl: 2h",
        )
        .unwrap();
        assert_eq!(configured.host_ip_whitelist.len(), 2);
        assert_eq!(configured.host_ip_cache_ttl, "2h");
        assert!(serde_yaml::from_str::<Config>("hostIPWhitelist: [example.com]").is_err());
    }

    #[test]
    fn cache_duration_validation() {
        let defaults = Settings::try_from(Config::default()).unwrap().cache;
        assert_eq!(
            (defaults.freshness, defaults.stale_retention),
            (86_400, 604_800)
        );
        let custom = serde_yaml::from_str::<Config>("freshness: 1h\nstale_retention: 0s").unwrap();
        assert_eq!(Settings::try_from(custom).unwrap().cache.stale_retention, 0);
        let short = serde_yaml::from_str::<Config>("freshness: 59m").unwrap();
        assert!(Settings::try_from(short).is_err());
    }
}
