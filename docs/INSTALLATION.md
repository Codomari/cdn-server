# Installation

## Prerequisites

- A Linux host with systemd for a live install.
- GNU Make, Rust and Cargo with Rust 2024 edition support, and a native linker and build toolchain.
- A reachable Redis-compatible database such as DragonflyDB. The service connects to `redis://127.0.0.1:6379` by default. For a different endpoint, set `REDIS_URL` in a systemd service override.
- Certbot on the service's executable search path if HTTPS uses the `certbot` provider.
- Sudo access for the default system install.

## Build and install

Run these commands from the project root.

```sh
make build
make install
```

`make build` compiles a locked release using `Cargo.lock` and publishes the executable at `./bin/cdn-server`. `make install` builds it under your current account if needed. Run it as `make install` so the build uses your own Cargo environment. For a nonroot user, the installer runs `sudo -v` before making host changes and prompts for credentials if needed. A root invocation runs the installation commands directly.

| Path | Purpose |
| --- | --- |
| `/etc/cdn-server/bin/cdn-server` | Installed release executable |
| `/etc/cdn-server/config.yaml` | Active configuration, initialized from the example only if missing |
| `/etc/cdn-server/config.example.yaml` | Reference configuration, replaced on each install |
| `/etc/cdn-server/storage/` | Default disk cache, writable by `cdn` |
| `/etc/cdn-server/certbot/` | Default Certbot state, writable by `cdn` |
| `/etc/cdn-server/certs/` | Directory accessible to `cdn` for operator-supplied certificates and keys |
| `/etc/systemd/system/cdn-server.service` | Installed systemd unit |

The installer ensures the `cdn` system user and group exist. The unit runs as `cdn:cdn` with working directory `/etc/cdn-server` and grants the service `CAP_NET_BIND_SERVICE` for ports 80 and 443. Installation reloads systemd, enables `cdn-server.service` at boot, and prints instructions to configure and start or restart it. Starting or restarting the service is a separate operator action.

## Configure and start

Edit `/etc/cdn-server/config.yaml` using the [configuration reference](CONFIGURATION.md). Configure the permitted origin hostnames and any public IP addresses you want to allow before serving traffic. For file-based TLS, supply the PEM files separately and give `cdn` read access, for example with a private key owned by `root:cdn` and mode `0640`. Relative certificate paths such as `certs/site.key` resolve under `/etc/cdn-server`.

Start the configured service, or use `restart` if it is already running and you have changed its configuration or executable.

```sh
sudo systemctl start cdn-server.service
systemctl status cdn-server.service
sudo journalctl -u cdn-server.service -n 50
```

## Stage an install offline

```sh
make install DESTDIR="$PWD/staging"
```

`DESTDIR` prefixes the installation paths. For example, the executable goes to `staging/etc/cdn-server/bin/cdn-server`. The staged unit and configuration retain their final runtime paths. Staging skips sudo, host user and group creation, systemd reload, and service enablement. For a network-free build, have the build dependencies available locally or build the release beforehand.
