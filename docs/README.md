# Haarness Router documentation

Start with the [README](../README.md) for what the router is and a one-minute quick start.

## Guides

- [Use with Haarness](haarness.md) — connect the router from Desktop, the terminal UI or the CLI, and pick its models.
- [Web UI](web-ui.md) — the embedded dashboard, its protection and process-local router keys.
- [Providers](providers.md) — built-in kinds, Vertex AI and Bedrock, capabilities, reasoning, adding an adapter.

## Reference

- [Configuration](configuration.md) — layers, `--set`/`--unset`, authentication and every key with its default.
- [API](api.md) — endpoints, headers, routing and fallback, request features, limits, errors and model discovery.
- [Observability](observability.md) — health, Prometheus metrics, logs and request correlation.
- [Security model](security.md) — credential boundaries, what is never logged or forwarded, deployment advice.

## Development

- [Development](development.md) — toolchain, build profiles, the mock fixture, the maintainers' launcher and the layout.
- [Testing and benchmarks](testing.md) — the deterministic suite, live smoke tests and the local load test.
- [Contributing](../CONTRIBUTING.md) — how a change gets merged.

## Records

- [2026-08-23 — local benchmark](records/2026-08-23-local-benchmark.md) — 10,000-stream soak and cancellation pass.
- [2026-09-12 — Haarness attach, live](records/2026-09-12-haarness-attach-live.md) — attached TUI and CLI through the router.
