# CDN server 0.5.0

This Rust service stores remote file bytes and metadata on disk. A Redis-compatible database such as DragonflyDB maps each cached URL path to its current ULID and keeps a copy of the generation's metadata for cache hits.

- [Installation](docs/INSTALLATION.md)
- [Configuration reference](docs/CONFIGURATION.md)
- [Changelog](CHANGELOG.md)
- [Contributing](CONTRIBUTING.md)
- [Testing](docs/TESTING.md)
- [Security reporting](SECURITY.md)

## Run locally

`config.example.yaml` shows the available settings with example hostnames and account details.

Start DragonflyDB in one terminal and the Rust app in another.

```sh
mkdir -p .dragonfly
dragonfly --bind=127.0.0.1 --port=6379 --dir=.dragonfly \
  --dbfilename=cdn-cache --force_epoll --proactor_threads=2
```

The app uses `redis://127.0.0.1:6379` for its path-to-ULID mapping. Set `REDIS_URL` to change the database address. The DragonflyDB command saves a snapshot on clean shutdown and reloads it on restart.

For a local test, copy `config.example.yaml` to `config.local.yaml`. Set `http.bind` to `127.0.0.1`, `http.port` to `8080`, and `storage` to `storage/`, then run `CONFIG_PATH=config.local.yaml cargo run` from the project directory. The examples below use this port.

## Routes

- `GET /api/v1/fetch?url=<url>` fills the cache and returns `204 No Content`. Use URL encoding for the `url` parameter.
- `GET /serve/<host>/<path>` returns the cached file or downloads and caches it on a miss. This route uses HTTPS for the origin.
- `GET /download/<host>/<path>` shares the `/serve` cache and sends files as `application/octet-stream` attachments with their original filenames. It follows at most one permitted redirect on the server. Download responses omit `Expires`, `ETag`, and `Cache-Control`.

Add `example.com` to `allowedHosts` and `bin` to `allowedExtensions` before trying these commands. The example URL has no filename extension.

```sh
curl -i -G --data-urlencode 'url=https://example.com/' \
  'http://127.0.0.1:8080/api/v1/fetch'
curl -i 'http://127.0.0.1:8080/serve/example.com/'
```

## License

Copyright © 2026 Anar K. Jafarov. Licensed under the [GNU Affero General Public License v3.0 only](LICENSE).
