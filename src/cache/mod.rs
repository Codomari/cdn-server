mod backend;
mod cleanup;
mod key;
mod metadata;
mod redis_index;
mod refresh;
mod storage;

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use redis::aio::MultiplexedConnection;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    config::CacheConfig,
    error::RequestError,
    files::FilePolicy,
    upstream::{self, OriginPolicy, Upstream},
};

use backend::Backend;
use redis_index::{DeleteMode, Pointer};
use refresh::{Generation, RefreshJob, RefreshQueue, RefreshWorkers};
use storage::Contents;

pub(crate) use storage::Storage;

pub(crate) struct CachedResponse {
    pub(crate) entry: Upstream,
    pub(crate) original_name: String,
    pub(crate) expires_at: u64,
    pub(crate) version_mismatch: bool,
}

#[derive(Clone)]
pub(crate) struct Cache {
    backend: Arc<Backend>,
    refresh: RefreshQueue,
}

pub(crate) struct CacheWorkers {
    backend: Arc<Backend>,
    refresh: RefreshWorkers,
}

impl CacheWorkers {
    pub(crate) async fn run(self, shutdown: CancellationToken) {
        tokio::join!(
            cleanup::run(&self.backend, shutdown.clone()),
            self.refresh.run(Arc::clone(&self.backend), shutdown),
        );
    }
}

impl Cache {
    pub(crate) fn new(
        redis: MultiplexedConnection,
        config: CacheConfig,
        origins: OriginPolicy,
        storage: Storage,
        files: FilePolicy,
    ) -> (Self, CacheWorkers) {
        let backend = Arc::new(Backend {
            redis,
            config,
            origins,
            storage,
            files,
        });
        let (refresh, workers) = RefreshQueue::new();
        (
            Self {
                backend: Arc::clone(&backend),
                refresh,
            },
            CacheWorkers {
                backend,
                refresh: workers,
            },
        )
    }

    pub(crate) async fn get_or_download(&self, url: Url) -> Result<CachedResponse, RequestError> {
        let backend = &self.backend;
        let filename = backend.files.filename(&url)?;
        let key = key::from_url(&url);
        let ip = backend.origins.approve(&backend.redis, &url).await?;
        let mut pointer = redis_index::lookup(&backend.redis, &key).await?;
        let requested = url.query().map(str::to_owned);
        if let Some(id) = pointer.id() {
            let directory = backend
                .storage
                .open_read(id)
                .await
                .map_err(|_| RequestError::StorageUnavailable)?;
            let contents = match directory {
                Some(directory) => backend.cached_contents(&directory, &key, id).await?,
                None => None,
            };
            match contents {
                Some((metadata, Contents::Data(body)))
                    if metadata.deadlines.usable(unix_seconds()) =>
                {
                    let mismatch = requested
                        .as_ref()
                        .is_some_and(|marker| *marker != metadata.version);
                    if mismatch || metadata.deadlines.should_refresh(unix_seconds()) {
                        let explicit = requested.is_some();
                        let marker = requested.unwrap_or_else(|| metadata.version.clone());
                        let mut refresh_url = url.clone();
                        refresh_url.set_query((!marker.is_empty()).then_some(&marker));
                        self.refresh.enqueue(RefreshJob {
                            key,
                            url: refresh_url,
                            filename,
                            observed: Generation {
                                deadlines: metadata.deadlines,
                                id: id.to_owned(),
                            },
                            marker,
                            explicit,
                        });
                    }
                    let entry = match metadata.redirect {
                        Some(redirect) => {
                            upstream::validate_cached_redirect(
                                &backend.redis,
                                &backend.origins,
                                &url,
                                &redirect,
                                backend.config.store_redirects,
                            )
                            .await?;
                            Upstream::Redirect(redirect)
                        }
                        None => Upstream::File(
                            upstream::content_type(metadata.content_type.as_bytes()),
                            body,
                        ),
                    };
                    return Ok(CachedResponse {
                        entry,
                        original_name: metadata.original_name,
                        expires_at: metadata.deadlines.expires_at,
                        version_mismatch: mismatch,
                    });
                }
                Some((metadata, Contents::TooLarge))
                    if metadata.deadlines.usable(unix_seconds()) => {}
                _ => {
                    redis_index::delete_generation(
                        &backend.redis,
                        &key,
                        id,
                        DeleteMode::AllowCurrent,
                    )
                    .await?;
                    pointer = Pointer::missing();
                }
            }
        }

        let entry = backend.download(url, ip, &filename).await?;
        let deadlines = backend
            .publish(
                &key,
                &filename,
                &entry,
                requested.as_deref().unwrap_or_default(),
                &pointer,
            )
            .await?;
        Ok(CachedResponse {
            entry,
            original_name: filename.original,
            expires_at: deadlines.expires_at,
            version_mismatch: false,
        })
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
