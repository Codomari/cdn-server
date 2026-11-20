use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{
    backend::Backend,
    metadata::Metadata,
    redis_index::{self, DeleteMode},
    storage::Sweep,
    unix_seconds,
};

const ORPHAN_GRACE: Duration = Duration::from_secs(360);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(30);
const CLEANUP_BATCH: usize = 128;

async fn sweep(backend: &Backend, cursor: &mut Sweep, shutdown: &CancellationToken) {
    for entry in backend.storage.sweep_page(cursor, CLEANUP_BATCH).await {
        if shutdown.is_cancelled() {
            return;
        }
        let staging = entry.staging;
        let id = entry.id.clone();
        let Ok(Some(directory)) = entry.lock().await else {
            continue;
        };
        let old = directory.older_than(ORPHAN_GRACE).unwrap_or(false);
        let remove = if staging {
            old
        } else {
            let (metadata, disk_json) = match redis_index::read_metadata(&backend.redis, &id).await
            {
                Ok(Some(metadata)) => (Some(metadata), None),
                Ok(None) => match directory.metadata_json().await {
                    Ok(json) => (json.as_deref().and_then(Metadata::parse), json),
                    Err(_) => continue,
                },
                Err(_) => continue,
            };
            match metadata {
                Some(metadata) => {
                    let key = &metadata.cache_key;
                    if !metadata.deadlines.usable(unix_seconds()) {
                        redis_index::delete_generation(
                            &backend.redis,
                            key,
                            &id,
                            DeleteMode::AllowCurrent,
                        )
                        .await
                        .is_ok_and(|removed| removed)
                    } else {
                        let orphan = if old {
                            match redis_index::delete_generation(
                                &backend.redis,
                                key,
                                &id,
                                DeleteMode::OrphanOnly,
                            )
                            .await
                            {
                                Ok(orphan) => orphan,
                                Err(_) => continue,
                            }
                        } else {
                            false
                        };
                        if !orphan && let Some(json) = disk_json {
                            let _ =
                                redis_index::warm_metadata(&backend.redis, key, &id, &json).await;
                        }
                        orphan
                    }
                }
                // With neither copy readable, the path is unknown. The next request
                // can clear a dangling pointer using its known path.
                None if old => redis_index::delete_metadata(&backend.redis, &id)
                    .await
                    .is_ok(),
                _ => false,
            }
        };
        if remove {
            let _ = directory.remove().await;
        }
    }
}

pub(super) async fn run(backend: &Backend, shutdown: CancellationToken) {
    let mut cursor = Sweep::default();
    while !shutdown.is_cancelled() {
        sweep(backend, &mut cursor, &shutdown).await;
        let delay = if cursor.in_progress() {
            Duration::from_millis(50)
        } else {
            CLEANUP_INTERVAL
        };
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
    }
}
