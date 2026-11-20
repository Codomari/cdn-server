use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::{
    sync::{Mutex as AsyncMutex, mpsc},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{error::RequestError, files::FileName};

use super::{
    backend::Backend,
    metadata::Deadlines,
    redis_index::{self, Pointer},
    unix_seconds,
};

const REFRESH_WORKERS: usize = 4;
const REFRESH_QUEUE_SIZE: usize = 64;
const REFRESH_BACKOFF: Duration = Duration::from_secs(60);
const MAX_BACKOFF_ENTRIES: usize = 1024;

pub(super) struct Generation {
    pub(super) deadlines: Deadlines,
    pub(super) id: String,
}

pub(super) struct RefreshWorkers {
    receiver: mpsc::Receiver<RefreshJob>,
    states: Arc<Mutex<RefreshStates>>,
}

impl RefreshWorkers {
    pub(super) async fn run(self, backend: Arc<Backend>, shutdown: CancellationToken) {
        let receiver = Arc::new(AsyncMutex::new(self.receiver));
        let mut tasks = JoinSet::new();
        for _ in 0..REFRESH_WORKERS {
            tasks.spawn(refresh_worker(
                Arc::clone(&receiver),
                Arc::clone(&self.states),
                Arc::clone(&backend),
                shutdown.clone(),
            ));
        }
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result {
                eprintln!("Cache refresh worker stopped: {error}");
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct RefreshQueue {
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

pub(super) struct RefreshJob {
    pub(super) key: String,
    pub(super) url: Url,
    pub(super) filename: FileName,
    pub(super) observed: Generation,
    pub(super) marker: String,
    pub(super) explicit: bool,
}

impl RefreshJob {
    fn retry_key(&self) -> (String, String) {
        (self.key.clone(), self.marker.clone())
    }
}

impl RefreshQueue {
    pub(super) fn new() -> (Self, RefreshWorkers) {
        let (sender, receiver) = mpsc::channel(REFRESH_QUEUE_SIZE);
        let states = Arc::new(Mutex::new(RefreshStates::default()));
        (
            Self {
                sender,
                states: Arc::clone(&states),
            },
            RefreshWorkers { receiver, states },
        )
    }

    pub(super) fn enqueue(&self, job: RefreshJob) {
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
    shutdown: CancellationToken,
) {
    loop {
        let mut job = tokio::select! {
            biased;
            () = shutdown.cancelled() => return,
            job = async { receiver.lock().await.recv().await } => match job {
                Some(job) => job,
                None => return,
            },
        };
        let mut follow_up = false;
        loop {
            let result = refresh_one(&backend, &mut job, follow_up).await;
            let Some(next) = states.lock().unwrap().finish(&job, result.is_err()) else {
                break;
            };
            if shutdown.is_cancelled() {
                return;
            }
            job = next;
            follow_up = true;
        }
    }
}

async fn refresh_one(
    backend: &Backend,
    job: &mut RefreshJob,
    follow_up: bool,
) -> Result<(), RequestError> {
    if !job.observed.deadlines.usable(unix_seconds()) {
        return Ok(());
    }
    if follow_up {
        let pointer = redis_index::lookup(&backend.redis, &job.key).await?;
        let Some(id) = pointer.id() else {
            return Ok(());
        };
        let Some(directory) = backend
            .storage
            .open_read(id)
            .await
            .map_err(|_| RequestError::StorageUnavailable)?
        else {
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
                &Pointer::for_refresh(&job.observed.id, job.observed.deadlines.hard_delete_at),
            )
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ulid::Ulid;

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
