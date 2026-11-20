#[derive(Debug)]
pub(crate) struct AllowedHosts(Vec<Vec<String>>);

impl AllowedHosts {
    pub(crate) fn new(patterns: Vec<String>) -> Result<Self, String> {
        let patterns = patterns
            .into_iter()
            .map(|pattern| {
                let normalized = pattern.strip_suffix('.').unwrap_or(&pattern);
                let labels: Vec<_> = normalized.split('.').collect();
                if normalized.len() > 253
                    || labels.len() < 2
                    || labels.iter().any(|label| !valid_label(label, true))
                {
                    return Err(format!("invalid allowedHosts pattern: {pattern:?}"));
                }
                Ok(labels.into_iter().map(str::to_ascii_lowercase).collect())
            })
            .collect::<Result<_, _>>()?;
        Ok(Self(patterns))
    }

    pub(crate) fn allows(&self, host: &str) -> bool {
        let host = host.strip_suffix('.').unwrap_or(host);
        let labels: Vec<_> = host.split('.').collect();
        if host.len() > 253 || labels.iter().any(|label| !valid_label(label, false)) {
            return false;
        }

        self.0.iter().any(|pattern| {
            pattern.len() == labels.len()
                && pattern
                    .iter()
                    .zip(&labels)
                    .all(|(pattern, label)| matches_label(pattern, &label.to_ascii_lowercase()))
        })
    }
}

fn valid_label(label: &str, wildcard: bool) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || (wildcard && byte == b'*'))
}

fn matches_label(pattern: &str, label: &str) -> bool {
    let Some((prefix, tail)) = pattern.split_once('*') else {
        return pattern == label;
    };

    let suffix = tail.rsplit('*').next().unwrap_or_default();
    let Some(mut remaining) = label
        .strip_prefix(prefix)
        .and_then(|label| label.strip_suffix(suffix))
    else {
        return false;
    };

    // Prefix and suffix are anchored. Match each literal between wildcards in order.
    let mut literals = tail.split('*');
    let _ = literals.next_back();
    for literal in literals {
        let Some((_, after)) = remaining.split_once(literal) else {
            return false;
        };
        remaining = after;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::AllowedHosts;

    #[test]
    fn exact_and_wildcard_hosts_match_whole_dns_labels() {
        let allowed = AllowedHosts::new(
            ["something.com", "*.az", "static.*.click", "cdn*-edge*.net"]
                .map(str::to_owned)
                .to_vec(),
        )
        .unwrap();
        for host in [
            "SOMETHING.com.",
            "example.az",
            "static.assets.click",
            "cdn-edge.net",
            "cdn1-edge2.net",
        ] {
            assert!(allowed.allows(host), "{host}");
        }
        for host in [
            "something.com.example",
            "sub.something.com",
            "nested.example.az",
            "az",
            "static..click",
            "static.assets.click.example",
            "notcdn1-edge2.net",
        ] {
            assert!(!allowed.allows(host), "{host}");
        }
    }

    #[test]
    fn invalid_configuration_and_empty_list() {
        for pattern in ["", "*", "https://example.com", "*.az:443", "a..az"] {
            assert!(
                AllowedHosts::new(vec![pattern.into()]).is_err(),
                "{pattern}"
            );
        }
        assert!(!AllowedHosts::new(vec![]).unwrap().allows("example.com"));
    }
}
