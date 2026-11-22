use std::{net::IpAddr, time::Duration};

use redis::aio::MultiplexedConnection;
use ulid::Ulid;
use url::Url;

use crate::{
    config::CacheConfig,
    error::RequestError,
    files::{FileName, FilePolicy},
    upstream::{self, OriginPolicy, Upstream},
};

use super::{
    metadata::{Deadlines, Metadata},
    redis_index::{self, Pointer},
    storage::{Contents, LockedDirectory, Storage},
    unix_seconds,
};

const PUBLICATION_WINDOW: Duration = Duration::from_secs(300);

pub(super) struct Backend {
    pub(super) redis: MultiplexedConnection,
    pub(super) config: CacheConfig,
    pub(super) origins: OriginPolicy,
    pub(super) storage: Storage,
    pub(super) files: FilePolicy,
}

impl Backend {
    pub(super) async fn download(
        &self,
        url: Url,
        ip: IpAddr,
        filename: &FileName,
    ) -> Result<Upstream, RequestError> {
        match upstream::download(&self.redis, &self.origins, url, ip, self.config).await? {
            Upstream::File(media_type, body) => {
                Ok(Upstream::File(filename.content_type(media_type), body))
            }
            redirect @ Upstream::Redirect(_) => Ok(redirect),
        }
    }

    pub(super) async fn active_metadata(
        &self,
        directory: &LockedDirectory,
        key: &str,
        id: &str,
    ) -> Result<Option<Metadata>, RequestError> {
        if let Some(metadata) = redis_index::read_metadata(&self.redis, id).await?
            && metadata.matches_key(key)
        {
            return Ok(Some(metadata));
        }
        let Some(json) = directory
            .metadata_json()
            .await
            .map_err(|_| RequestError::StorageUnavailable)?
        else {
            return Ok(None);
        };
        let Some(metadata) = Metadata::parse(&json).filter(|metadata| metadata.matches_key(key))
        else {
            return Ok(None);
        };
        if metadata.deadlines.usable(unix_seconds()) {
            redis_index::warm_metadata(&self.redis, key, id, &json).await?;
        }
        Ok(Some(metadata))
    }

    pub(super) async fn read_cached_contents(
        &self,
        key: &str,
        id: &str,
    ) -> Result<Option<(Metadata, Contents)>, RequestError> {
        let Some(directory) = self
            .storage
            .open_read(id)
            .await
            .map_err(|_| RequestError::StorageUnavailable)?
        else {
            return Ok(None);
        };
        let Some(metadata) = self.active_metadata(&directory, key, id).await? else {
            return Ok(None);
        };
        if !metadata.deadlines.usable(unix_seconds()) {
            return Ok(None);
        }
        let contents = directory
            .read(self.config.max_file_size)
            .await
            .map_err(|_| RequestError::StorageUnavailable)?;
        Ok(Some((metadata, contents)))
    }

    pub(super) async fn publish(
        &self,
        key: &str,
        filename: &FileName,
        entry: &Upstream,
        marker: &str,
        expected: &Pointer,
    ) -> Result<Deadlines, RequestError> {
        if !expected.supports_publication() {
            return Err(RequestError::UnsupportedCacheKeyType);
        }
        let id = Ulid::new().to_string();
        let now = unix_seconds();
        let (content_type, bytes, redirect) = match entry {
            Upstream::File(media_type, bytes) => (
                String::from_utf8_lossy(media_type.as_bytes()).into_owned(),
                bytes.as_slice(),
                None,
            ),
            Upstream::Redirect(redirect) => (String::new(), &[][..], Some(redirect.clone())),
        };
        let metadata = Metadata {
            cache_key: key.to_owned(),
            original_name: filename.original.clone(),
            content_type,
            redirect,
            version: marker.to_owned(),
            deadlines: Deadlines::new(now, self.config),
        };
        let (directory, json) = self
            .storage
            .write(&id, bytes, &metadata)
            .await
            .map_err(|_| RequestError::StorageUnavailable)?;

        // A lost Redis response can mean the SET committed. The sweeper resolves it.
        let deadline = now + PUBLICATION_WINDOW.as_secs();
        let published =
            redis_index::publish(&self.redis, key, &id, json, expected, deadline).await?;
        if !published {
            let _ = directory.remove().await;
        }
        Ok(metadata.deadlines)
    }
}
