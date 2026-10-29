# Development

How to build, run and change Haarness Router. The [build and run skill](../.skills/haarness-router-build-run/SKILL.md)
is the short version agents follow; [testing](testing.md) covers the suites.

## Toolchain

A stable Rust toolchain: CI uses 1.94.0 (`.github/workflows/ci.yml`) and the workspace declares 1.88 as its
minimum (`rust-version` in `Cargo.toml`). The repository pins no `rust-toolchain.toml`. Builds need no
Node.js: the dashboard's HTML, CSS and JavaScript in `crates/router-server/assets` are embedded into the
binary with `include_str!`, so a UI change needs a Rust rebuild and a restart.

## Build and run

```bash
cargo build --bin haarness-router                     # dev profile, incremental
OPENAI_API_KEY=… target/debug/haarness-router         # no file: one provider per key set
OPENAI_API_KEY=… ANTHROPIC_API_KEY=… GEMINI_API_KEY=… \
  target/debug/haarness-router --config config.example.toml --port 8080
```

Without `--config` and without a `config.toml` in the current folder, the router
[starts from the environment](configuration.md#starting-without-a-configuration-file); the repository root
has no `config.toml`, so `cargo run --bin haarness-router` does that. On a terminal it prints a startup
summary and readable logs; `--log-format json` shows what a service manager would get.
`haarness-router --help` lists every flag. Three profiles are configured in `Cargo.toml`:

| Profile | Use | Binary |
| --- | --- | --- |
| `dev` | Everyday iteration; line-tables-only debug info, incremental. Tests use it too. | `target/debug/haarness-router` |
| `release` | Optimised (thin LTO, one codegen unit, stripped) for benchmarks and deployment. | `target/release/haarness-router` |
| `debugger` | `dev` with full debug info, for inspecting variables in a native debugger. | `target/debugger/haarness-router` |

Check a running instance with `curl http://127.0.0.1:8080/healthz`; health alone does not prove that
provider calls work.

## Without provider credentials

The `mock-provider` binary is a deterministic OpenAI-compatible SSE server, and `tests/config.mock.toml`
routes `mock/test` to it with the Web UI off:

```bash
cargo run --bin mock-provider -- --bind 127.0.0.1:9090
HAARNESS_ROUTER_KEY=local-router-key MOCK_PROVIDER_KEY=unused-local-provider-key \
  cargo run --bin haarness-router -- --config tests/config.mock.toml --port 8080
```

`mock-provider` also takes `--hold-ms` (keep each stream open) and `--heartbeat-ms` (default 15,000). These
are fixture keys, and a passing mock run says nothing about real providers.

## The maintainers' launcher

`scripts/run-stackforward` is how the maintainers run the router against real providers on their own
machines. It:

1. creates or reuses the ignored, mode-0600 `.env` in the repository root with a random
   `HAARNESS_ROUTER_KEY` and separate `HAARNESS_UI_USERNAME`/`HAARNESS_UI_PASSWORD` for the dashboard;
2. reads the OpenAI, Anthropic and Gemini keys from a private YAML file (`openai_api_key`,
   `claude_api_key`, `gemini_api_key`) named by `HAARNESS_STACKFORWARD_SECRETS`, without printing or
   copying them;
3. builds the binary and starts it with `config.stackforward.toml`, or the file named by
   `HAARNESS_ROUTER_CONFIG` or `--config`, passing every other argument through.

```bash
./scripts/run-stackforward                                # release build
HAARNESS_ROUTER_PROFILE=dev ./scripts/run-stackforward    # dev build (also: debugger)
./scripts/run-stackforward --require-key                  # require HAARNESS_ROUTER_KEY from .env
./scripts/run-stackforward --port 9090                    # or --host / --bind / --set …
./scripts/run-stackforward --config config.local.toml
```

Open the dashboard with the `.env` credentials, and `source .env` before running the client examples.
To try the start without a configuration file, run the binary yourself with only the provider keys in its
environment and no `config.toml` in its folder.
Without the credential YAML the launcher stops; use the plain build-and-run steps above instead.
`python3 scripts/test-launcher-profile.py` checks the launcher's profile selection and argument forwarding
without credentials or compilation.

## Windows

`.cargo/config.toml` links the C runtime statically for the `x86_64-pc-windows-msvc` and
`aarch64-pc-windows-msvc` targets, so `haarness-router.exe` starts on a machine without the Visual C++
Redistributable. Cross-checking from macOS or Linux needs the MSVC headers, for example through
`cargo xwin`, because the TLS stack compiles C code.

## Repository layout

```text
crates/router-protocol/   public Chat Completions request and response types
crates/router-core/       configuration, capabilities, auth, routing, fallback, circuit breakers
crates/router-providers/  provider adapters, SSE, PDFs, model discovery
crates/router-server/     Axum HTTP/SSE surface, metrics, embedded dashboard (assets/)
crates/haarness-router/   the server binary, and live-smoke, mock-provider, load-test in src/bin/
config*.toml              example configurations and the launcher's configuration
tests/                    configurations for the mock fixture and the load test
scripts/                  the launcher and the live Haarness attach check
examples/                 OpenAI SDK clients in Python and JavaScript
```

See [providers](providers.md#adding-a-provider) for the boundaries between crates and how to add an
adapter.

## Before a pull request

Run the deterministic suite from [testing](testing.md) — at least formatting, Clippy and the workspace
tests, which CI runs on every push — and update `README.md` and these pages when behaviour or
configuration changes. See [CONTRIBUTING.md](../CONTRIBUTING.md).
