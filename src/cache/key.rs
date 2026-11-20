use percent_encoding::{percent_decode_str, percent_encode, utf8_percent_encode};
use url::Url;

use crate::files::URL_COMPONENT;

use super::storage::valid_ulid;

pub(super) fn from_url(url: &Url) -> String {
    let authority = &url[url::Position::BeforeHost..url::Position::BeforePath];
    let mut key = String::new();
    if url.scheme() == "http" {
        key.push_str("http|");
    }
    key.extend(utf8_percent_encode(authority, URL_COMPONENT));
    for segment in url.path().split(['/', '\\']).skip(1) {
        key.push(':');
        let decoded: Vec<_> = percent_decode_str(segment).collect();
        key.extend(percent_encode(&decoded, URL_COMPONENT));
    }
    // Keep URL keys out of the metadata namespace, even for a host named "file".
    if key
        .strip_prefix("file:")
        .and_then(|tail| tail.strip_suffix(":metadata"))
        .is_some_and(valid_ulid)
    {
        format!("https|{key}")
    } else {
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream;

    #[test]
    fn canonical_cache_keys() {
        let key = |url| from_url(&upstream::parse(url).unwrap());
        assert_eq!(
            key("https://Example.com/some\\static/image.jpg"),
            "example.com:some:static:image.jpg"
        );
        assert_eq!(key("https://example.com/a?t=123"), "example.com:a");
        assert_eq!(
            key("https://example.com/a?v=other#fragment"),
            key("https://example.com/a")
        );
        assert_ne!(key("https://example.com/a"), key("http://example.com/a"));
        assert_ne!(
            key("https://example.com/a:b"),
            key("https://example.com/a/b")
        );
        assert_eq!(
            key("https://example.com/café%20+%20notes.txt"),
            "example.com:caf%C3%A9%20%2B%20notes.txt"
        );
    }

    #[test]
    fn keeps_url_keys_out_of_the_metadata_namespace() {
        let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let url = Url::parse(&format!("https://file/{id}/metadata")).unwrap();
        assert_eq!(from_url(&url), format!("https|file:{id}:metadata"));
    }
}
