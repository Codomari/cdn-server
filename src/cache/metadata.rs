use serde::{Deserialize, Serialize};

use crate::{
    config::CacheConfig,
    upstream::{StoredRedirect, valid_marker},
};

pub(super) const METADATA_LIMIT: usize = 32 * 1024;
const REFRESH_WINDOW: u64 = 600;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Deadlines {
    pub(super) expires_at: u64,
    pub(super) hard_delete_at: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Metadata {
    pub(super) cache_key: String,
    pub(super) original_name: String,
    pub(super) content_type: String,
    pub(super) version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) redirect: Option<StoredRedirect>,
    #[serde(flatten)]
    pub(super) deadlines: Deadlines,
}

impl Deadlines {
    pub(super) fn new(now: u64, config: CacheConfig) -> Self {
        let expires_at = now + config.freshness;
        Self {
            expires_at,
            hard_delete_at: expires_at + config.stale_retention,
        }
    }

    pub(super) fn usable(self, now: u64) -> bool {
        now < self.hard_delete_at && self.expires_at <= self.hard_delete_at
    }

    pub(super) fn should_refresh(self, now: u64) -> bool {
        self.expires_at.saturating_sub(now) <= REFRESH_WINDOW
    }
}

impl Metadata {
    pub(super) fn parse(json: &[u8]) -> Option<Self> {
        let metadata: Self = serde_json::from_slice(json).ok()?;
        (metadata.deadlines.expires_at <= metadata.deadlines.hard_delete_at).then_some(metadata)
    }

    pub(super) fn matches_key(&self, key: &str) -> bool {
        self.cache_key == key && (self.version.is_empty() || valid_marker(&self.version))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_window_includes_stale_but_hard_expiry_is_unusable() {
        let deadlines = Deadlines {
            expires_at: 1000,
            hard_delete_at: 2000,
        };
        assert!(!deadlines.should_refresh(399));
        assert!(deadlines.should_refresh(400));
        assert!(deadlines.should_refresh(1000));
        assert!(deadlines.usable(1999));
        assert!(!deadlines.usable(2000));
    }
}
