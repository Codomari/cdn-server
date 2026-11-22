# Configuration

The installed service reads `/etc/cdn-server/config.yaml`. For a direct run, `CONFIG_PATH` selects the file. Without it, the app reads `config.yaml` from the working directory and uses built-in defaults if that file is missing. Unknown YAML fields are rejected. See [config.example.yaml](../config.example.yaml) for an example with both TLS providers.

## Listeners

| Field | Default | Behavior |
| --- | --- | --- |
| `http.enabled` | `true` | Enables the plaintext listener. Keep it enabled for Certbot HTTP-01. |
| `http.bind` | `0.0.0.0` | IPv4 or IPv6 address to bind. The default listens on all IPv4 interfaces. |
| `http.port` | `80` | TCP port for HTTP. |
| `https.enabled` | `false` | Enables TLS and requires a TLS configuration with at least one host. At least one of HTTP or HTTPS must be enabled. |
| `https.bind` | `0.0.0.0` | IPv4 or IPv6 address to bind. The default listens on all IPv4 interfaces. |
| `https.port` | `443` | TCP port for HTTPS. |
| `https.tls` | Unset | Settings for certificate selection and management. |

## TLS

| Field | Default | Behavior |
| --- | --- | --- |
| `https.tls.hosts` | Required in a TLS block | List of per-host certificate configurations. The server selects a certificate by the client's TLS Server Name Indication (SNI). |
| `https.tls.hosts[].name` | Required | Unique, explicit DNS hostname in ASCII or Punycode. Matching ignores case. Wildcard entries and IP literals are rejected. |
| `https.tls.hosts[].provider` | Required | `file` loads operator-supplied PEM files. `certbot` manages a certificate using ACME HTTP-01. Both providers can be used in the same list. |
| `https.tls.hosts[].cert` | Required for `file` | PEM certificate chain path. |
| `https.tls.hosts[].key` | Required for `file` | Matching PEM private key path, readable by the service user. |
| `https.tls.hosts[].account` | Required for `certbot` | Email address for the ACME account. Certbot reuses the registered account in the host's existing state directory. |
| `https.tls.stateDir` | `certbot/` | Shared writable location for Certbot's per-host configuration, certificates, work files, logs, and HTTP-01 webroot. |
| `https.tls.staging` | `false` | Uses the Let's Encrypt staging environment when true. Staging and production state use separate subdirectories. Trust validation by ordinary clients fails for staging certificates. |

Relative certificate and state paths resolve from the working directory, which is `/etc/cdn-server` under systemd. Paths starting with `/` are absolute. If HTTPS is disabled, the TLS block still has to match the YAML schema when present. Certificate loading, hostname validation, and Certbot processing run when HTTPS is enabled.

For each Certbot hostname, public DNS must lead to a server that routes public TCP port 80 to the app's HTTP listener. The app serves `/.well-known/acme-challenge/<token>` and invokes Certbot in webroot mode as the service user. It loads available certificates at startup, checks for renewal every 12 hours after a successful check, and reloads Certbot certificates in process. TLS handshakes for a host need a loaded certificate. Restart the service after changing file-based certificates.

## Origins and redirects

| Field | Default | Behavior |
| --- | --- | --- |
| `allowedHosts` | `[]` in code | Exact hostnames and wildcard patterns. Matching ignores case, and `*` matches within a DNS label. For example, `'static.*.example.org'` matches `static.media.example.org`. Matching DNS hostnames can use any public address they resolve to. Quote patterns in YAML. |
| `hostIPWhitelist` | `[]` | Exact public IPv4 and IPv6 addresses. DNS hostnames outside `allowedHosts` require an address from this list. Fresh lookups also prefer these addresses for hostnames in `allowedHosts`. Literal IP origins must match both lists. |
| `hostIPCacheTtl` | `1d` | Positive whole-second lifetime for the approved address stored at `host:<hostname>:ip` in Redis. Examples include `30m`, `12h`, and `1d`. |
| `storeRedirects` | `false` | When enabled, caches eligible upstream redirects and returns their status and `Location` from `/serve`. |

If both lists are empty, all origins are blocked. A hostname outside `allowedHosts` gets HTTP `403` when its public address fails the IP whitelist check. A lookup with no public address or an invalid cached IP gets HTTP `444`. On a Redis hit, the app checks the cached address against the current rules and reuses it if permitted. Fresh lookups cache the approved address only. Downloads connect to that address while preserving the original hostname for TLS verification.

Upstream redirects with status 301, 302, 303, 307, or 308 return HTTP `444` when `storeRedirects` is disabled. When enabled, the destination must pass the same origin rules and URL validation as a direct request. An invalid or disallowed target also returns `444`. HTTPS-to-HTTP downgrades are rejected. The app resolves relative destinations against the origin and stores the absolute destination. `/serve` returns the redirect to the client. `/download` follows one permitted redirect and sends the final file as an attachment. A second redirect returns `444`.

Only one optional version parameter is accepted per origin URL, with a value of 1 to 64 characters. Use `t` for ASCII digits or `v` for ASCII letters, digits, dots, underscores, or hyphens. The marker is stored in disk metadata and left out of the Redis path key. A different explicit marker queues a background replacement for the same path.

## Storage and expiration

| Field | Default | Behavior |
| --- | --- | --- |
| `storage` | `/etc/cdn-server/storage` | Cache base directory. Absolute paths resolve from the filesystem root. Relative paths resolve from the app's working directory. For local development, set `storage: storage/`. Make the directory writable by the service user. |
| `maxFilesize` | `8MiB` | Positive per-file limit for downloads and disk reads. Accepts a byte count or a whole number with a case-insensitive unit such as `B`, `KB`, `MB`, `GB`, `KiB`, `MiB`, or `GiB`. |
| `allowedExtensions` | `[]` | Permitted filename extensions. An empty list permits any extension that passes the deny list. |
| `deniedExtensions` | `[]` | Blocked filename extensions. This list takes precedence. |
| `freshness` | `24h` | Time from download to `expiresAt`. Use a whole-second duration of at least `1h`. A request within ten minutes of expiry or after expiry queues a background refresh. |
| `stale_retention` | `7d` | Additional time after `expiresAt` until `hardDeleteAt`. Use a whole-second duration. `0s` removes the stale serving window. |

Extension matching ignores case and accepts an optional leading dot in configuration. Suffixes can have multiple dot-separated parts, such as `tar.gz`, and can be up to 32 characters after the optional leading dot. Each part uses ASCII letters or digits. Names without a usable extension are checked as `bin`. The example config permits common image formats, CSS, JavaScript, and JSON. Common image types, CSS, and JavaScript receive media types based on the extension. An `application/octet-stream` response uses the original filename for the download attachment.

Each generation lives under `<storage>/files/<first 2 ULID characters>/<next 2>/<remaining 22>/`. It contains `data.bin` and `metadata.json`. Metadata includes the cache key, original filename, media type, version marker, expiration and deletion timestamps in Unix seconds, and redirect data where applicable. Redis stores the path-to-ULID mapping without a TTL. Both slash forms separate path components with colons in the key, and components are percent-encoded for Unicode and symbols. For example, the key for `https://media.example.com/files/café.jpg` is `media.example.com:files:caf%C3%A9.jpg`. HTTP origin keys carry an `http|` prefix.

File responses from `/serve` use `expiresAt` for the `Expires` header and the remaining freshness for `Cache-Control`. Stale files have `max-age=0` and require revalidation. A version mismatch also requires revalidation while the replacement is downloading. The weak `ETag` uses the file bytes' CRC32 and length. A matching `If-None-Match` returns `304 Not Modified` with the cache headers. Redirect responses use `Cache-Control: no-store`.

Redis keeps a copy of `metadata.json` at `file:<ULID>:metadata`. A cache hit uses that copy. If the copy is missing, the app reads `metadata.json` from disk and restores the copy for the current ULID.

The service continues serving the cached generation during background refresh and after a refresh failure until the hard deadline. A successful expiration or version refresh publishes a new ULID and metadata together and removes the previous Redis copy. At the hard deadline, the entry becomes a cache miss. Cleanup removes the expired generation and its Redis metadata copy and deletes the path mapping if it still points to that generation.
