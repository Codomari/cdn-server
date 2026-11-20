use std::{
    collections::HashMap,
    io,
    net::IpAddr,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::http::StatusCode;
use redis::aio::MultiplexedConnection;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use ulid::Ulid;
use url::Url;

use crate::{
    HttpError,
    config::CacheConfig,
    file_policy::FileName,
    storage::{
        Contents, Deadlines, LockedDirectory, METADATA_LIMIT, Metadata, Storage, Sweep, valid_ulid,
    },
    upstream::{self, Upstream},
};

const REFRESH_WINDOW: u64 = 600;
const REFRESH_WORKERS: usize = 4;
const REFRESH_QUEUE_SIZE: usize = 64;
const REFRESH_BACKOFF: Duration = Duration::from_secs(60);
const MAX_BACKOFF_ENTRIES: usize = 1024;

const PUBLICATION_WINDOW: Duration = Duration::from_secs(300);
const ORPHAN_GRACE: Duration = Duration::from_secs(360);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(30);
const CLEANUP_BATCH: usize = 128;

const LOOKUP: &str = r#"
local kind = redis.call('TYPE', KEYS[1]).ok
if kind == 'string' then return {kind, redis.call('GET', KEYS[1])} end
return {kind, ''}
"#;

const PUBLISH: &str = r#"
local kind = redis.call('TYPE', KEYS[1]).ok
if kind ~= ARGV[1] then return 0 end
if kind == 'string' and redis.call('GET', KEYS[1]) ~= ARGV[2] then return 0 end
if tonumber(redis.call('TIME')[1]) >= tonumber(ARGV[4]) then return 0 end
redis.call('MSET', KEYS[1], ARGV[3], KEYS[2], ARGV[5])
if KEYS[3] then redis.call('DEL', KEYS[3]) end
return 1
"#;

const READ_METADATA: &str = r#"
if redis.call('TYPE', KEYS[1]).ok == 'string'
    and redis.call('STRLEN', KEYS[1]) <= tonumber(ARGV[1]) then
    return redis.call('GET', KEYS[1])
end
return false
"#;

const WARM_METADATA: &str = r#"
if redis.call('TYPE', KEYS[1]).ok == 'string'
    and redis.call('GET', KEYS[1]) == ARGV[1] then
    redis.call('SET', KEYS[2], ARGV[2])
end
return 1
"#;

const DELETE_GENERATION: &str = r#"
if redis.call('TYPE', KEYS[1]).ok == 'string'
    and redis.call('GET', KEYS[1]) == ARGV[1] then
    if ARGV[2] == 'orphan' then return 0 end
    redis.call('DEL', KEYS[1])
end
redis.call('DEL', KEYS[2])
return 1
"#;

fn metadata_key(id: &str) -> String {
    format!("file:{id}:metadata")
}

struct Pointer {
    kind: String,
    value: String,
    publish_before: Option<u64>,
}

impl Pointer {
    fn missing() -> Self {
        Self {
            kind: "none".into(),
            value: String::new(),
            publish_before: None,
        }
    }

    fn id(&self) -> Option<&str> {
        (self.kind == "string" && valid_ulid(&self.value)).then_some(&self.value)
    }
}

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

struct Backend {
    redis: MultiplexedConnection,
    config: CacheConfig,
    origins: upstream::OriginPolicy,
    storage: Storage,
}

impl Deadlines {
    fn new(now: u64, config: CacheConfig) -> Self {
        let expires_at = now + config.freshness;
        Self {
            expires_at,
            hard_delete_at: expires_at + config.stale_retention,
        }
    }

    fn usable(self, now: u64) -> bool {
        now < self.hard_delete_at && self.expires_at <= self.hard_delete_at
    }

    fn should_refresh(self, now: u64) -> bool {
        self.expires_at.saturating_sub(now) <= REFRESH_WINDOW
    }
}

struct Generation {
    deadlines: Deadlines,
    id: String,
}

impl Cache {
    pub(crate) fn new(
        redis: MultiplexedConnection,
        config: CacheConfig,
        origins: upstream::OriginPolicy,
        storage: Storage,
    ) -> Self {
        let backend = Arc::new(Backend {
            redis,
            config,
            origins,
            storage,
        });
        let (sender, receiver) = mpsc::channel(REFRESH_QUEUE_SIZE);
        let receiver = Arc::new(AsyncMutex::new(receiver));
        let states = Arc::new(Mutex::new(RefreshStates::default()));
        for _ in 0..REFRESH_WORKERS {
            tokio::spawn(refresh_worker(
                Arc::clone(&receiver),
                Arc::clone(&states),
                Arc::clone(&backend),
            ));
        }
        tokio::spawn(cleanup_worker(Arc::downgrade(&backend)));
        Self {
            backend,
            refresh: RefreshQueue { sender, states },
        }
    }

    pub(crate) async fn get_or_download(
        &self,
        key: String,
        url: Url,
        filename: FileName,
    ) -> Result<CachedResponse, HttpError> {
        // Keep URL keys out of the metadata namespace, even for a host named "file".
        let key = if key
            .strip_prefix("file:")
            .and_then(|tail| tail.strip_suffix(":metadata"))
            .is_some_and(valid_ulid)
        {
            format!("https|{key}")
        } else {
            key
        };
        let backend = &self.backend;
        let ip = backend.origins.approve(&backend.redis, &url).await?;
        let mut pointer = backend.lookup(&key).await?;
        let requested = url.query().map(str::to_owned);
        if let Some(id) = pointer.id() {
            let directory = backend.storage.open_read(id).await.map_err(storage_error)?;
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
                    backend.delete_generation(&key, id, false).await?;
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

impl Backend {
    async fn download(
        &self,
        url: Url,
        ip: IpAddr,
        filename: &FileName,
    ) -> Result<Upstream, HttpError> {
        match upstream::download(&self.redis, &self.origins, url, ip, self.config).await? {
            Upstream::File(media_type, body) => {
                Ok(Upstream::File(filename.content_type(media_type), body))
            }
            redirect @ Upstream::Redirect(_) => Ok(redirect),
        }
    }

    async fn lookup(&self, key: &str) -> Result<Pointer, HttpError> {
        let (kind, value) = redis::cmd("EVAL")
            .arg(LOOKUP)
            .arg(1)
            .arg(key)
            .query_async(&mut self.redis.clone())
            .await
            .map_err(cache_error)?;
        Ok(Pointer {
            kind,
            value,
            publish_before: None,
        })
    }

    async fn redis_metadata(&self, id: &str) -> Result<Option<Metadata>, HttpError> {
        let json: Option<Vec<u8>> = redis::cmd("EVAL")
            .arg(READ_METADATA)
            .arg(1)
            .arg(metadata_key(id))
            .arg(METADATA_LIMIT)
            .query_async(&mut self.redis.clone())
            .await
            .map_err(cache_error)?;
        Ok(json.as_deref().and_then(parse_metadata))
    }

    async fn active_metadata(
        &self,
        directory: &LockedDirectory,
        key: &str,
        id: &str,
    ) -> Result<Option<Metadata>, HttpError> {
        if let Some(metadata) = self.redis_metadata(id).await?
            && metadata_matches(&metadata, key)
        {
            return Ok(Some(metadata));
        }
        let Some(json) = directory.metadata_json().await.map_err(storage_error)? else {
            return Ok(None);
        };
        let Some(metadata) =
            parse_metadata(&json).filter(|metadata| metadata_matches(metadata, key))
        else {
            return Ok(None);
        };
        if metadata.deadlines.usable(unix_seconds()) {
            self.warm_metadata(key, id, &json).await?;
        }
        Ok(Some(metadata))
    }

    async fn warm_metadata(&self, key: &str, id: &str, json: &[u8]) -> Result<(), HttpError> {
        redis::cmd("EVAL")
            .arg(WARM_METADATA)
            .arg(2)
            .arg(key)
            .arg(metadata_key(id))
            .arg(id)
            .arg(json)
            .query_async::<()>(&mut self.redis.clone())
            .await
            .map_err(cache_error)
    }

    async fn cached_contents(
        &self,
        directory: &LockedDirectory,
        key: &str,
        id: &str,
    ) -> Result<Option<(Metadata, Contents)>, HttpError> {
        let Some(metadata) = self.active_metadata(directory, key, id).await? else {
            return Ok(None);
        };
        if !metadata.deadlines.usable(unix_seconds()) {
            return Ok(None);
        }
        let contents = directory
            .read(self.config.max_file_size)
            .await
            .map_err(storage_error)?;
        Ok(Some((metadata, contents)))
    }

    async fn delete_generation(
        &self,
        key: &str,
        id: &str,
        orphan_only: bool,
    ) -> Result<bool, HttpError> {
        redis::cmd("EVAL")
            .arg(DELETE_GENERATION)
            .arg(2)
            .arg(key)
            .arg(metadata_key(id))
            .arg(id)
            .arg(if orphan_only { "orphan" } else { "remove" })
            .query_async(&mut self.redis.clone())
            .await
            .map_err(cache_error)
    }

    async fn publish(
        &self,
        key: &str,
        filename: &FileName,
        entry: &Upstream,
        marker: &str,
        expected: &Pointer,
    ) -> Result<Deadlines, HttpError> {
        if !matches!(expected.kind.as_str(), "none" | "hash" | "string") {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "cache key has an unsupported type",
            ));
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
            .map_err(storage_error)?;

        // A lost Redis response can mean the SET committed. The sweeper resolves it.
        let previous_metadata = expected.id().map(metadata_key);
        let mut command = redis::cmd("EVAL");
        command
            .arg(PUBLISH)
            .arg(2 + usize::from(previous_metadata.is_some()))
            .arg(key)
            .arg(metadata_key(&id))
            .arg(previous_metadata);
        let published: bool = command
            .arg(&expected.kind)
            .arg(&expected.value)
            .arg(&id)
            .arg(
                (now + PUBLICATION_WINDOW.as_secs())
                    .min(expected.publish_before.unwrap_or(u64::MAX)),
            )
            .arg(json)
            .query_async(&mut self.redis.clone())
            .await
            .map_err(cache_error)?;
        if !published {
            let _ = directory.remove().await;
        }
        Ok(metadata.deadlines)
    }

    async fn sweep(&self, cursor: &mut Sweep) {
        for entry in self.storage.sweep_page(cursor, CLEANUP_BATCH).await {
            let staging = entry.staging;
            let id = entry.id.clone();
            let Ok(Some(directory)) = entry.lock().await else {
                continue;
            };
            let old = directory.older_than(ORPHAN_GRACE).unwrap_or(false);
            let remove = if staging {
                old
            } else {
                let (metadata, disk_json) = match self.redis_metadata(&id).await {
                    Ok(Some(metadata)) => (Some(metadata), None),
                    Ok(None) => match directory.metadata_json().await {
                        Ok(json) => (json.as_deref().and_then(parse_metadata), json),
                        Err(_) => continue,
                    },
                    Err(_) => continue,
                };
                match metadata {
                    Some(metadata) => {
                        let key = &metadata.cache_key;
                        if !metadata.deadlines.usable(unix_seconds()) {
                            self.delete_generation(key, &id, false)
                                .await
                                .is_ok_and(|removed| removed)
                        } else {
                            let orphan = if old {
                                match self.delete_generation(key, &id, true).await {
                                    Ok(orphan) => orphan,
                                    Err(_) => continue,
                                }
                            } else {
                                false
                            };
                            if !orphan && let Some(json) = disk_json {
                                let _ = self.warm_metadata(key, &id, &json).await;
                            }
                            orphan
                        }
                    }
                    // With neither copy readable, the path is unknown. The next request
                    // can clear a dangling pointer using its known path.
                    None if old => redis::cmd("DEL")
                        .arg(metadata_key(&id))
                        .query_async::<()>(&mut self.redis.clone())
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
}

async fn cleanup_worker(backend: Weak<Backend>) {
    let mut cursor = Sweep::default();
    loop {
        let Some(current) = backend.upgrade() else {
            return;
        };
        current.sweep(&mut cursor).await;
        drop(current);
        let delay = if cursor.in_progress() {
            Duration::from_millis(50)
        } else {
            CLEANUP_INTERVAL
        };
        tokio::time::sleep(delay).await;
    }
}

fn parse_metadata(json: &[u8]) -> Option<Metadata> {
    let metadata: Metadata = serde_json::from_slice(json).ok()?;
    (metadata.deadlines.expires_at <= metadata.deadlines.hard_delete_at).then_some(metadata)
}

fn metadata_matches(metadata: &Metadata, key: &str) -> bool {
    metadata.cache_key == key
        && (metadata.version.is_empty() || upstream::valid_marker(&metadata.version))
}

const STORAGE_UNAVAILABLE: HttpError =
    (StatusCode::SERVICE_UNAVAILABLE, "file storage unavailable");

fn storage_error(_: io::Error) -> HttpError {
    STORAGE_UNAVAILABLE
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn cache_error(_: redis::RedisError) -> HttpError {
    (StatusCode::SERVICE_UNAVAILABLE, "cache unavailable")
}

#[derive(Clone)]
struct RefreshQueue {
    sender: mpsc::Sender<RefreshJob>,
    states: Arc<Mutex<RefreshStates>>,
}

#[derive(Default)]
struct RefreshStates {
    pending: HashMap<String, PendingRefresh>,
    backoff: HashMap<(String, String), Instant>,
}

struct PendingRefresh {
    marker: String,
    follow_up: Option<RefreshJob>,
}

struct RefreshJob {
    key: String,
    url: Url,
    filename: FileName,
    observed: Generation,
    marker: String,
    explicit: bool,
}

impl RefreshJob {
    fn retry_key(&self) -> (String, String) {
        (self.key.clone(), self.marker.clone())
    }
}

impl RefreshQueue {
    fn enqueue(&self, job: RefreshJob) {
        let mut states = self.states.lock().unwrap();
        let now = Instant::now();
        let retry_key = job.retry_key();
        if states
            .backoff
            .get(&retry_key)
            .is_some_and(|until| *until > now)
        {
            return;
        }
        states.backoff.remove(&retry_key);

        if let Some(pending) = states.pending.get_mut(&job.key) {
            if job.explicit {
                pending.follow_up = (job.marker != pending.marker).then_some(job);
            }
            return;
        }

        // Reserve first so a full queue leaves the path eligible on later requests.
        if let Ok(permit) = self.sender.try_reserve() {
            states.pending.insert(
                job.key.clone(),
                PendingRefresh {
                    marker: job.marker.clone(),
                    follow_up: None,
                },
            );
            permit.send(job);
        }
    }
}

impl RefreshStates {
    fn finish(&mut self, job: &RefreshJob, failed: bool) -> Option<RefreshJob> {
        if failed {
            let now = Instant::now();
            if self.backoff.len() >= MAX_BACKOFF_ENTRIES {
                self.backoff.retain(|_, until| *until > now);
                if self.backoff.len() >= MAX_BACKOFF_ENTRIES
                    && let Some(oldest) = self
                        .backoff
                        .iter()
                        .min_by_key(|(_, until)| *until)
                        .map(|(key, _)| key.clone())
                {
                    self.backoff.remove(&oldest);
                }
            }
            self.backoff.insert(job.retry_key(), now + REFRESH_BACKOFF);
        }

        if let Some(pending) = self.pending.get_mut(&job.key)
            && let Some(next) = pending.follow_up.take()
        {
            pending.marker.clone_from(&next.marker);
            return Some(next);
        }
        self.pending.remove(&job.key);
        None
    }
}

async fn refresh_worker(
    receiver: Arc<AsyncMutex<mpsc::Receiver<RefreshJob>>>,
    states: Arc<Mutex<RefreshStates>>,
    backend: Arc<Backend>,
) {
    loop {
        let Some(mut job) = receiver.lock().await.recv().await else {
            return;
        };
        let mut follow_up = false;
        loop {
            let result = refresh_one(&backend, &mut job, follow_up).await;
            let Some(next) = states.lock().unwrap().finish(&job, result.is_err()) else {
                break;
            };
            job = next;
            follow_up = true;
        }
    }
}

async fn refresh_one(
    backend: &Backend,
    job: &mut RefreshJob,
    follow_up: bool,
) -> Result<(), HttpError> {
    if !job.observed.deadlines.usable(unix_seconds()) {
        return Ok(());
    }
    if follow_up {
        let pointer = backend.lookup(&job.key).await?;
        let Some(id) = pointer.id() else {
            return Ok(());
        };
        let Some(directory) = backend.storage.open_read(id).await.map_err(storage_error)? else {
            return Ok(());
        };
        let Some(metadata) = backend.active_metadata(&directory, &job.key, id).await? else {
            return Ok(());
        };
        if !metadata.deadlines.usable(unix_seconds()) || metadata.version == job.marker {
            return Ok(());
        }
        job.observed = Generation {
            deadlines: metadata.deadlines,
            id: id.to_owned(),
        };
    }

    let ip = backend.origins.approve(&backend.redis, &job.url).await?;
    let entry = backend.download(job.url.clone(), ip, &job.filename).await?;
    if job.observed.deadlines.usable(unix_seconds()) {
        backend
            .publish(
                &job.key,
                &job.filename,
                &entry,
                &job.marker,
                &Pointer {
                    kind: "string".into(),
                    value: job.observed.id.clone(),
                    publish_before: Some(job.observed.deadlines.hard_delete_at),
                },
            )
            .await?;
    }
    Ok(())
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

    #[test]
    fn queue_coalesces_tracks_latest_explicit_and_backs_off_by_marker() {
        let (sender, mut receiver) = mpsc::channel(1);
        let queue = RefreshQueue {
            sender,
            states: Arc::new(Mutex::new(RefreshStates::default())),
        };
        let enqueue = |key: &str, marker: &str, explicit| {
            queue.enqueue(RefreshJob {
                key: key.to_owned(),
                url: Url::parse("https://something.com/").unwrap(),
                filename: FileName {
                    original: "download".into(),
                    extension: "bin".into(),
                },
                observed: Generation {
                    deadlines: Deadlines {
                        expires_at: 1000,
                        hard_delete_at: 2000,
                    },
                    id: Ulid::new().to_string(),
                },
                marker: marker.into(),
                explicit,
            })
        };
        enqueue("first", "v=1", true);
        enqueue("second", "", false);
        assert!(!queue.states.lock().unwrap().pending.contains_key("second"));
        let first = receiver.try_recv().unwrap();
        enqueue("first", "v=2", true);
        enqueue("first", "v=3", true);
        enqueue("first", "", false);
        assert!(receiver.try_recv().is_err());
        let next = queue.states.lock().unwrap().finish(&first, true).unwrap();
        assert_eq!(next.marker, "v=3");
        assert!(queue.states.lock().unwrap().finish(&next, false).is_none());
        enqueue("first", "v=1", true);
        assert!(receiver.try_recv().is_err());
        enqueue("first", "v=2", true);
        assert_eq!(receiver.try_recv().unwrap().marker, "v=2");
    }
}
