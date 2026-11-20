# Contributing

Open a GitHub issue for a reproducible bug or a feature proposal. For a security issue, follow the [private reporting instructions](SECURITY.md).

1. Fork the repository and create a branch for your change.
2. Use the [installation guide](docs/INSTALLATION.md) and copy `config.example.yaml` to `config.local.yaml` when testing locally.
3. Update the relevant documentation and add a focused regression test when you change behavior.
4. Run the checks in the [testing guide](docs/TESTING.md).
5. Open a pull request with the reason for the change, verification results, and any effects on configuration, caching, or service startup.

Use redacted configuration excerpts and logs in issues and pull requests. Keep private keys, live settings, and cache files out of commits. Contributions are licensed under [AGPL-3.0-only](LICENSE).
