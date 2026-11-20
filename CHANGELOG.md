# Changelog

## 0.2.1

- Added `/download/<host>/<path>` for `application/octet-stream` attachments with the final file's original filename. The route follows at most one permitted redirect and omits client cache headers.
- Added AGPL-3.0 licensing, contribution and testing guides, private security reporting, GitHub Actions CI, and issue and pull request templates.

## 0.1.0

### HTTP API and responses

- Added `GET /api/v1/fetch?url=<url>` to fill the cache and return `204 No Content`, and `GET /serve/<host>/<path>` to serve a cached file or download it over HTTPS on a miss.
- Added media types for images, CSS, and JavaScript, plus attachment filenames for binary responses. Original Unicode filenames are preserved for downloads.
- Added `Expires` from `expiresAt`, freshness-based `Cache-Control`, and weak `ETag` values from the file’s CRC32 and byte length. Matching `If-None-Match` requests return `304 Not Modified`.
- Added optional redirect caching. When `storeRedirects` is enabled, the server validates the destination and replays the upstream status and `Location` with `Cache-Control: no-store`. Disabled or disallowed redirects return HTTP `444`.

### Storage and refresh

- Store files as `data.bin` beside `metadata.json` in sharded ULID directories. Storage paths can be absolute or relative to the working directory. Installed deployments default to `/etc/cdn-server/storage`.
- Use Redis or DragonflyDB for path-to-ULID mappings and a `file:<ULID>:metadata` copy of the disk metadata. Cache hits use the Redis copy, and missing or invalid copies are restored from disk.
- Separate Redis path components with colons and percent-encode embedded symbols and Unicode. HTTP and HTTPS origins have distinct keys.
- Accept one optional version marker, numeric `t` or textual `v`. Changes queue background replacement at the same path. Unversioned requests can reuse the cached version.
- Default to 24 hours of freshness with a configurable minimum of one hour. Requests within ten minutes of expiry or after expiry queue a background refresh. Failed refreshes retain the old file for a configurable seven days after expiry by default.
- Publish refreshed generations and metadata atomically. At hard expiry, cleanup removes the disk generation and its Redis entries without removing a newer replacement.

### Origin controls

- Added exact and wildcard hostname allowlists. Allowed DNS hostnames can use public IP addresses. Literal IP origins must also match `hostIPWhitelist`.
- Cache one resolved IP per hostname at `host:<hostname>:ip` for one day by default. Validate cached IPs and pin outbound connections to the selected public address while preserving the hostname for TLS verification. Invalid or disallowed IPs return HTTP `444`.
- Added configurable file-size limits and case-insensitive extension allowlists and denylists. Deny rules take precedence and support compound suffixes such as `tar.gz`. The example config allows common image formats, CSS, JavaScript, and JSON.

### Configuration and installation

- Added YAML-configured HTTP and HTTPS listeners, defaulting to `0.0.0.0` on ports `80` and `443`. HTTPS is opt-in.
- Added per-host TLS certificate selection using SNI. Hosts can use PEM files or Certbot with ACME HTTP-01 issuance, renewal checks, and in-process certificate reload. Certbot staging is configurable.
- Added a sanitized example config and installation and configuration guides. Git ignores the active config, certificates, and generated build and runtime directories.
- Added `make build` for the release binary at `./bin/cdn-server` and `make install` for `/etc/cdn-server`. Installation checks for root privileges, uses sudo when needed, preserves the active config, refreshes the example, and supports staging via `DESTDIR`.
- Added a systemd unit that runs as `cdn` with `CAP_NET_BIND_SERVICE`. System installation creates the account if needed, reloads systemd, and enables the unit at boot. Operators start or restart the service separately.
