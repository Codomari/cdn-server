use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};
use tokio::{
    fs::{self, OpenOptions, ReadDir},
    io::{AsyncReadExt, AsyncWriteExt},
};
use ulid::Ulid;

const DATA: &str = "data.bin";
const METADATA: &str = "metadata.json";
const STAGING_PREFIX: &str = ".stage-";
pub(crate) const METADATA_LIMIT: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Deadlines {
    pub(crate) expires_at: u64,
    pub(crate) hard_delete_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct StoredRedirect {
    pub(crate) status: u16,
    pub(crate) location: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Metadata {
    pub(crate) cache_key: String,
    pub(crate) original_name: String,
    pub(crate) content_type: String,
    pub(crate) version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) redirect: Option<StoredRedirect>,
    #[serde(flatten)]
    pub(crate) deadlines: Deadlines,
}

#[derive(Clone)]
pub(crate) struct Storage {
    root: Arc<Path>,
}

impl Storage {
    pub(crate) async fn new(path: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&path).await?;
        let root = fs::canonicalize(path).await?.join("files");
        create_directory(&root).await?;
        Ok(Self { root: root.into() })
    }

    pub(crate) async fn write(
        &self,
        id: &str,
        bytes: &[u8],
        metadata: &Metadata,
    ) -> io::Result<(LockedDirectory, Vec<u8>)> {
        let target = self.path(id)?;
        let parent = target.parent().ok_or_else(invalid_name)?;
        create_directory(&self.root.join(&id[..2])).await?;
        create_directory(parent).await?;
        let staging = parent.join(format!("{STAGING_PREFIX}{}", &id[4..]));
        let json = serde_json::to_vec(metadata)?;
        if json.len() > METADATA_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "metadata too large",
            ));
        }

        fs::create_dir(&staging).await?;
        let mut directory = LockedDirectory::open(staging, false)
            .await?
            .ok_or_else(|| io::Error::other("staging directory unavailable"))?;
        write_file(&directory.path.join(DATA), bytes).await?;
        write_file(&directory.path.join(METADATA), &json).await?;
        sync_directory(&directory.path).await?;
        match fs::symlink_metadata(&target).await {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(_) => return Err(io::ErrorKind::AlreadyExists.into()),
        }
        fs::rename(&directory.path, &target).await?;
        sync_directory(parent).await?;
        directory.path = target;
        Ok((directory, json))
    }

    pub(crate) async fn open_read(&self, id: &str) -> io::Result<Option<LockedDirectory>> {
        let path = self.path(id)?;
        for shard in [
            self.root.join(&id[..2]),
            self.root.join(&id[..2]).join(&id[2..4]),
            path.clone(),
        ] {
            match fs::symlink_metadata(shard).await {
                Ok(info) if info.is_dir() => {}
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid storage shard",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        for _ in 0..3 {
            match LockedDirectory::open(path.clone(), true).await {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                result => return result,
            }
        }
        LockedDirectory::open(path, true).await
    }

    fn path(&self, id: &str) -> io::Result<PathBuf> {
        if !valid_ulid(id) {
            return Err(invalid_name());
        }
        Ok(self.root.join(&id[..2]).join(&id[2..4]).join(&id[4..]))
    }

    pub(crate) async fn sweep_page(&self, cursor: &mut Sweep, budget: usize) -> Vec<SweepEntry> {
        let mut leaves = Vec::new();
        if cursor.stack.is_empty() {
            let Ok(entries) = fs::read_dir(&self.root).await else {
                return leaves;
            };
            cursor.stack.push(entries);
        }
        for _ in 0..budget {
            let depth = cursor.stack.len();
            let Some(entries) = cursor.stack.last_mut() else {
                break;
            };
            let Ok(Some(entry)) = entries.next_entry().await else {
                cursor.stack.pop();
                continue;
            };
            if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if depth < 3 {
                if name.len() == 2
                    && name.bytes().all(|byte| byte.is_ascii_alphanumeric())
                    && let Ok(entries) = fs::read_dir(&path).await
                {
                    cursor.stack.push(entries);
                }
                continue;
            }
            let (leaf, staging) = match name.strip_prefix(STAGING_PREFIX) {
                Some(leaf) => (leaf, true),
                None => (name, false),
            };
            let mut shards = path.iter().rev().skip(1);
            let second = shards
                .next()
                .and_then(|part| part.to_str())
                .unwrap_or_default();
            let first = shards
                .next()
                .and_then(|part| part.to_str())
                .unwrap_or_default();
            let id = format!("{first}{second}{leaf}");
            if valid_ulid(&id) {
                leaves.push(SweepEntry { id, path, staging });
            }
        }
        leaves
    }
}

#[derive(Default)]
pub(crate) struct Sweep {
    stack: Vec<ReadDir>,
}

impl Sweep {
    pub(crate) fn in_progress(&self) -> bool {
        !self.stack.is_empty()
    }
}

pub(crate) struct SweepEntry {
    pub(crate) id: String,
    path: PathBuf,
    pub(crate) staging: bool,
}

impl SweepEntry {
    pub(crate) async fn lock(self) -> io::Result<Option<LockedDirectory>> {
        match LockedDirectory::open(self.path, false).await {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            result => result,
        }
    }
}

// The lock follows the directory inode across rename and remains held through Redis publication.
pub(crate) struct LockedDirectory {
    path: PathBuf,
    file: File,
}

impl LockedDirectory {
    async fn open(path: PathBuf, shared: bool) -> io::Result<Option<Self>> {
        let file = match fs::File::open(&path).await {
            Ok(file) => file.into_std().await,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let lock = if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        };
        match lock {
            Ok(()) => Ok(Some(Self { path, file })),
            Err(std::fs::TryLockError::WouldBlock) => Err(io::ErrorKind::WouldBlock.into()),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }

    pub(crate) async fn metadata_json(&self) -> io::Result<Option<Vec<u8>>> {
        match read_file(&self.path.join(METADATA), METADATA_LIMIT).await? {
            Contents::Data(json) => Ok(Some(json)),
            Contents::Missing | Contents::TooLarge => Ok(None),
        }
    }

    pub(crate) async fn read(&self, limit: usize) -> io::Result<Contents> {
        read_file(&self.path.join(DATA), limit).await
    }

    pub(crate) fn older_than(&self, age: Duration) -> io::Result<bool> {
        let modified = self.file.metadata()?.modified()?;
        Ok(SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|elapsed| elapsed >= age))
    }

    pub(crate) async fn remove(self) -> io::Result<()> {
        for name in [DATA, METADATA] {
            match fs::remove_file(self.path.join(name)).await {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        match fs::remove_dir(&self.path).await {
            Ok(()) => sync_directory(self.path.parent().ok_or_else(invalid_name)?).await,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {
                sync_directory(&self.path).await
            }
            Err(error) => Err(error),
        }
    }
}

pub(crate) enum Contents {
    Missing,
    TooLarge,
    Data(Vec<u8>),
}

async fn read_file(path: &Path, limit: usize) -> io::Result<Contents> {
    let info = match fs::symlink_metadata(path).await {
        Ok(info) => info,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Contents::Missing),
        Err(error) => return Err(error),
    };
    if !info.is_file() {
        return Ok(Contents::Missing);
    }
    if info.len() > limit as u64 {
        return Ok(Contents::TooLarge);
    }
    let file = match fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Contents::Missing),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > limit {
        return Ok(Contents::TooLarge);
    }
    Ok(Contents::Data(bytes))
}

async fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    file.write_all(bytes).await?;
    file.sync_all().await
}

async fn create_directory(path: &Path) -> io::Result<()> {
    match fs::create_dir(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if !fs::symlink_metadata(path).await?.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid storage shard",
                ));
            }
        }
        Err(error) => return Err(error),
    }
    sync_directory(path.parent().ok_or_else(invalid_name)?).await
}

async fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path).await?.sync_all().await
}

fn invalid_name() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid storage ULID")
}

pub(crate) fn valid_ulid(id: &str) -> bool {
    id.len() == 26 && Ulid::from_string(id).is_ok_and(|ulid| ulid.to_string() == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn published_pair_is_locked_and_cleanup_preserves_unrelated_files() {
        let root = std::env::temp_dir().join(format!("cdn-storage-test-{}", Ulid::new()));
        let storage = Storage::new(root.clone()).await.unwrap();
        let id = Ulid::new().to_string();
        let metadata = Metadata {
            cache_key: "example.com:caf%C3%A9.jpg".into(),
            original_name: "café.jpg".into(),
            content_type: "image/jpeg".into(),
            version: "t=12".into(),
            redirect: None,
            deadlines: Deadlines {
                expires_at: 100,
                hard_delete_at: 200,
            },
        };
        let (writer, _) = storage.write(&id, b"image", &metadata).await.unwrap();
        let mut cursor = Sweep::default();
        let mut entries = storage.sweep_page(&mut cursor, 128).await;
        let entry = entries.pop().unwrap();
        assert_eq!(entry.id, id);
        assert!(!entry.staging);
        assert!(entry.lock().await.unwrap().is_none());
        let leaf = writer.path.clone();
        drop(writer);

        let reader = storage.open_read(&id).await.unwrap().unwrap();
        assert!(matches!(reader.read(4).await.unwrap(), Contents::TooLarge));
        assert!(matches!(reader.read(5).await.unwrap(), Contents::Data(body) if body == b"image"));
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(leaf.join(METADATA)).await.unwrap()).unwrap();
        assert_eq!(json["cacheKey"], metadata.cache_key);
        assert_eq!(json["originalName"], metadata.original_name);
        assert_eq!(json["contentType"], "image/jpeg");
        assert_eq!(json["version"], "t=12");
        assert_eq!(json["expiresAt"], 100);
        assert_eq!(json["hardDeleteAt"], 200);
        let mut entries = storage.sweep_page(&mut cursor, 128).await;
        assert!(entries.pop().unwrap().lock().await.unwrap().is_none());
        drop(reader);

        fs::remove_file(leaf.join(METADATA)).await.unwrap();
        fs::write(leaf.join("unrelated.txt"), b"keep")
            .await
            .unwrap();
        let mut entries = storage.sweep_page(&mut cursor, 128).await;
        let directory = entries.pop().unwrap().lock().await.unwrap().unwrap();
        assert!(directory.metadata_json().await.unwrap().is_none());
        directory.remove().await.unwrap();
        assert!(!leaf.join(DATA).exists());
        assert_eq!(fs::read(leaf.join("unrelated.txt")).await.unwrap(), b"keep");
        fs::remove_dir_all(root).await.unwrap();
    }
}
