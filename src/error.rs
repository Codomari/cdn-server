#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RequestError {
    InvalidUrl,
    InvalidVersionMarker,
    InvalidFilename,
    ExtensionDenied,
    HostDenied,
    IpDenied,
    RedirectDenied,
    UpstreamNotFound,
    UpstreamFailure,
    FileTooLarge,
    UpstreamTimeout,
    CacheUnavailable,
    StorageUnavailable,
    UnsupportedCacheKeyType,
    TooManyRequests,
}
