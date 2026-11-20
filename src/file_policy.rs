use std::collections::HashSet;

use axum::http::{HeaderValue, StatusCode};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use url::Url;

use crate::HttpError;

pub(crate) const URL_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');
const INVALID_FILENAME: HttpError = (StatusCode::BAD_REQUEST, "invalid filename");

pub(crate) struct FilePolicy {
    allowed: HashSet<String>,
    denied: HashSet<String>,
}

#[derive(Clone)]
pub(crate) struct FileName {
    pub(crate) original: String,
    pub(crate) extension: String,
}

impl FilePolicy {
    pub(crate) fn new(allowed: Vec<String>, denied: Vec<String>) -> Result<Self, String> {
        Ok(Self {
            allowed: extensions(allowed)?,
            denied: extensions(denied)?,
        })
    }

    pub(crate) fn filename(&self, url: &Url) -> Result<FileName, HttpError> {
        let path = url.path();
        let last = path.rsplit('/').next().unwrap_or_default();
        let decoded = percent_decode_str(last)
            .decode_utf8()
            .map_err(|_| INVALID_FILENAME)?;
        if decoded.len() > 255
            || decoded.contains(['/', '\\'])
            || decoded.chars().any(char::is_control)
        {
            return Err(INVALID_FILENAME);
        }
        let original = if decoded.is_empty() {
            "download".to_owned()
        } else {
            decoded.into_owned()
        };
        let extension = original
            .rsplit_once('.')
            .map(|(_, extension)| extension)
            .filter(|extension| valid_extension(extension))
            .unwrap_or("bin")
            .to_ascii_lowercase();

        let lowercase = original.to_ascii_lowercase();
        let matches = |rules: &HashSet<String>| {
            rules.contains(&extension)
                || lowercase
                    .match_indices('.')
                    .any(|(dot, _)| rules.contains(&lowercase[dot + 1..]))
        };
        if matches(&self.denied) || (!self.allowed.is_empty() && !matches(&self.allowed)) {
            return Err((StatusCode::FORBIDDEN, "file extension is not permitted"));
        }
        Ok(FileName {
            original,
            extension,
        })
    }
}

fn extensions(entries: Vec<String>) -> Result<HashSet<String>, String> {
    entries
        .into_iter()
        .map(|entry| {
            let extension = entry.strip_prefix('.').unwrap_or(&entry);
            if extension.len() <= 32 && extension.split('.').all(valid_extension) {
                Ok(extension.to_ascii_lowercase())
            } else {
                Err(format!("invalid extension in configuration: {entry:?}"))
            }
        })
        .collect()
}

fn valid_extension(value: &str) -> bool {
    !value.is_empty() && value.len() <= 32 && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

impl FileName {
    pub(crate) fn content_type(&self, upstream: HeaderValue) -> HeaderValue {
        match self.extension.as_str() {
            "css" => HeaderValue::from_static("text/css"),
            "js" | "mjs" => HeaderValue::from_static("text/javascript"),
            extension => mime_guess::from_ext(extension)
                .iter_raw()
                .find(|value| value.starts_with("image/"))
                .map_or(upstream, HeaderValue::from_static),
        }
    }
}

pub(crate) fn attachment(filename: &str) -> HeaderValue {
    let fallback: String = filename
        .chars()
        .map(|c| match c {
            '"' | '\\' => '_',
            c if c.is_ascii() && !c.is_ascii_control() => c,
            _ => '_',
        })
        .collect();
    let encoded = utf8_percent_encode(filename, URL_COMPONENT);
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compound_denial_takes_precedence_over_a_simple_allow_rule() {
        let policy = FilePolicy::new(vec!["gz".into()], vec![".tar.gz".into()]).unwrap();
        let archive = Url::parse("https://example.com/archive.TAR%2EGZ").unwrap();
        assert_eq!(
            policy.filename(&archive).err().unwrap().0,
            StatusCode::FORBIDDEN
        );
        let compressed = Url::parse("https://example.com/log.gz").unwrap();
        assert!(policy.filename(&compressed).is_ok());
    }

    #[test]
    fn filenames_keep_unicode_and_apply_extension_policy() {
        let policy =
            FilePolicy::new(vec![".CSS".into(), "png".into()], vec!["png".into()]).unwrap();
        let url = Url::parse("https://example.com/café%20%2B%20notes.CSS").unwrap();
        let file = policy.filename(&url).unwrap();
        assert_eq!(file.original, "café + notes.CSS");
        assert_eq!(file.extension, "css");
        assert_eq!(
            file.content_type(HeaderValue::from_static("application/octet-stream")),
            "text/css"
        );
        let png = Url::parse("https://example.com/example.PNG").unwrap();
        assert_eq!(
            policy.filename(&png).err().unwrap().0,
            StatusCode::FORBIDDEN
        );
        assert!(
            attachment(&file.original)
                .to_str()
                .unwrap()
                .contains("filename*=UTF-8''caf%C3%A9%20%2B%20notes.CSS")
        );
    }
}
