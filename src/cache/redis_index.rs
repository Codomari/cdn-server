use redis::aio::MultiplexedConnection;

use crate::error::RequestError;

use super::{
    metadata::{METADATA_LIMIT, Metadata},
    storage::valid_ulid,
};

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

pub(super) struct Pointer {
    kind: String,
    value: String,
    publish_before: Option<u64>,
}

impl Pointer {
    pub(super) fn missing() -> Self {
        Self {
            kind: "none".into(),
            value: String::new(),
            publish_before: None,
        }
    }

    pub(super) fn for_refresh(id: &str, deadline: u64) -> Self {
        Self {
            kind: "string".into(),
            value: id.to_owned(),
            publish_before: Some(deadline),
        }
    }

    pub(super) fn supports_publication(&self) -> bool {
        matches!(self.kind.as_str(), "none" | "hash" | "string")
    }

    pub(super) fn id(&self) -> Option<&str> {
        (self.kind == "string" && valid_ulid(&self.value)).then_some(&self.value)
    }
}

pub(super) enum DeleteMode {
    AllowCurrent,
    OrphanOnly,
}

pub(super) async fn lookup(
    redis: &MultiplexedConnection,
    key: &str,
) -> Result<Pointer, RequestError> {
    let (kind, value) = redis::cmd("EVAL")
        .arg(LOOKUP)
        .arg(1)
        .arg(key)
        .query_async(&mut redis.clone())
        .await
        .map_err(|_| RequestError::CacheUnavailable)?;
    Ok(Pointer {
        kind,
        value,
        publish_before: None,
    })
}

pub(super) async fn read_metadata(
    redis: &MultiplexedConnection,
    id: &str,
) -> Result<Option<Metadata>, RequestError> {
    let json: Option<Vec<u8>> = redis::cmd("EVAL")
        .arg(READ_METADATA)
        .arg(1)
        .arg(metadata_key(id))
        .arg(METADATA_LIMIT)
        .query_async(&mut redis.clone())
        .await
        .map_err(|_| RequestError::CacheUnavailable)?;
    Ok(json.as_deref().and_then(Metadata::parse))
}

pub(super) async fn warm_metadata(
    redis: &MultiplexedConnection,
    key: &str,
    id: &str,
    json: &[u8],
) -> Result<(), RequestError> {
    redis::cmd("EVAL")
        .arg(WARM_METADATA)
        .arg(2)
        .arg(key)
        .arg(metadata_key(id))
        .arg(id)
        .arg(json)
        .query_async::<()>(&mut redis.clone())
        .await
        .map_err(|_| RequestError::CacheUnavailable)
}

pub(super) async fn delete_generation(
    redis: &MultiplexedConnection,
    key: &str,
    id: &str,
    mode: DeleteMode,
) -> Result<bool, RequestError> {
    redis::cmd("EVAL")
        .arg(DELETE_GENERATION)
        .arg(2)
        .arg(key)
        .arg(metadata_key(id))
        .arg(id)
        .arg(match mode {
            DeleteMode::OrphanOnly => "orphan",
            DeleteMode::AllowCurrent => "remove",
        })
        .query_async(&mut redis.clone())
        .await
        .map_err(|_| RequestError::CacheUnavailable)
}

pub(super) async fn publish(
    redis: &MultiplexedConnection,
    key: &str,
    id: &str,
    json: Vec<u8>,
    expected: &Pointer,
    deadline: u64,
) -> Result<bool, RequestError> {
    let previous_metadata = expected.id().map(metadata_key);
    let mut command = redis::cmd("EVAL");
    command
        .arg(PUBLISH)
        .arg(2 + usize::from(previous_metadata.is_some()))
        .arg(key)
        .arg(metadata_key(id))
        .arg(previous_metadata);
    command
        .arg(&expected.kind)
        .arg(&expected.value)
        .arg(id)
        .arg(deadline.min(expected.publish_before.unwrap_or(u64::MAX)))
        .arg(json)
        .query_async(&mut redis.clone())
        .await
        .map_err(|_| RequestError::CacheUnavailable)
}

pub(super) async fn delete_metadata(
    redis: &MultiplexedConnection,
    id: &str,
) -> Result<(), RequestError> {
    redis::cmd("DEL")
        .arg(metadata_key(id))
        .query_async(&mut redis.clone())
        .await
        .map_err(|_| RequestError::CacheUnavailable)
}
