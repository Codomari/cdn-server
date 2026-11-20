mod listeners;

use std::{
    collections::HashSet, env, error::Error, fs, io, net::IpAddr, path::PathBuf, time::Duration,
};

use serde::Deserialize;

use crate::{
    files::FilePolicy,
    upstream::{AllowedHosts, OriginPolicy},
};

use listeners::{HttpConfig, HttpsConfig};
pub(crate) use listeners::{Listeners, TlsHost};

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    http: HttpConfig,
    https: HttpsConfig,
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
    pub(crate) cache: CacheConfig,
    pub(crate) origins: OriginPolicy,
    pub(crate) storage: PathBuf,
    pub(crate) files: FilePolicy,
}

pub(crate) fn load() -> Result<Settings, Box<dyn Error>> {
    let explicit = env::var_os("CONFIG_PATH");
    let path = explicit.clone().unwrap_or_else(|| "config.yaml".into());
    let mut config: Config = match fs::read_to_string(&path) {
        Ok(contents) => serde_yaml::from_str(&contents)?,
        Err(error) if explicit.is_none() && error.kind() == io::ErrorKind::NotFound => {
            Config::default()
        }
        Err(error) => return Err(error.into()),
    };
    if config.storage.as_os_str().is_empty() {
        return Err("storage must be a directory path".into());
    }
    let allowed_hosts = AllowedHosts::new(std::mem::take(&mut config.allowed_hosts))?;
    let ttl = humantime::parse_duration(&config.host_ip_cache_ttl)
        .map_err(|error| format!("invalid hostIPCacheTtl: {error}"))?;
    if ttl.as_secs() == 0 || ttl.subsec_nanos() != 0 || ttl.as_secs() > i64::MAX as u64 / 2000 {
        return Err(
            "hostIPCacheTtl must be a positive whole-second duration in the supported range".into(),
        );
    }
    let origins = OriginPolicy {
        allowed_hosts,
        whitelist: std::mem::take(&mut config.host_ip_whitelist),
        ttl_seconds: ttl.as_secs(),
    };
    let files = FilePolicy::new(
        std::mem::take(&mut config.allowed_extensions),
        std::mem::take(&mut config.denied_extensions),
    )?;
    let storage = std::mem::take(&mut config.storage);
    Ok(Settings {
        listeners: Listeners::from_config(&config.http, &mut config.https)?,
        cache: CacheConfig::try_from(config)?,
        origins,
        storage,
        files,
    })
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

impl TryFrom<Config> for CacheConfig {
    type Error = Box<dyn Error>;

    fn try_from(config: Config) -> Result<Self, Self::Error> {
        let freshness = humantime::parse_duration(&config.freshness)?;
        let stale_retention = humantime::parse_duration(&config.stale_retention)?;
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
            max_file_size: config.max_filesize.bytes()?,
            store_redirects: config.store_redirects,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let defaults =
            CacheConfig::try_from(serde_yaml::from_str::<Config>("{}").unwrap()).unwrap();
        assert_eq!(
            (defaults.freshness, defaults.stale_retention),
            (86_400, 604_800)
        );
        let custom = serde_yaml::from_str::<Config>("freshness: 1h\nstale_retention: 0s").unwrap();
        assert_eq!(CacheConfig::try_from(custom).unwrap().stale_retention, 0);
        let short = serde_yaml::from_str::<Config>("freshness: 59m").unwrap();
        assert!(CacheConfig::try_from(short).is_err());
    }
}
