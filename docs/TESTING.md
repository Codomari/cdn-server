# Testing

Run the same checks as GitHub Actions from the repository root with a current stable Rust toolchain and the `rustfmt` and `clippy` components installed.

```sh
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
```

The automated suite covers configuration parsing, origin and extension rules, cache keys, refresh and expiry behavior, storage, redirects, and HTTP cache headers. It uses local fixtures and needs no Redis instance, Certbot account, or private configuration.

## Local HTTP smoke test

Follow the [local setup in the README](../README.md). Use a loopback listener and a test DragonflyDB or Redis instance. Add `example.com` to `allowedHosts` and `bin` to `allowedExtensions` to test the extensionless example URL.

```sh
curl -i -G --data-urlencode 'url=https://example.com/' \
  'http://127.0.0.1:8080/api/v1/fetch'
curl -i 'http://127.0.0.1:8080/serve/example.com/'
curl -i 'http://127.0.0.1:8080/download/example.com/'
```

Expect `204` for prefetch and `200` with a response body, `Expires`, `Cache-Control`, and `ETag` for serving. The download response contains the same bytes as an `application/octet-stream` attachment and omits those three cache headers. Repeat the serve request with the returned `ETag` as `If-None-Match` and expect an empty `304` response. Use separate cache storage and Redis data from the running service.

Use a local PEM certificate for HTTPS integration checks. For install verification, use the [staged installation procedure](INSTALLATION.md#stage-an-install-offline).
