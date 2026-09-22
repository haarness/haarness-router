# Testing and benchmarks

## Deterministic suite

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release --workspace --all-targets
cargo audit
cargo deny check
```

The black-box tests launch local provider servers and exercise authentication isolation, non-stream reduction, usage, retryable fallback, and authorization failures. Native-adapter unit fixtures cover tool/reasoning translation, thought signatures, schema conversion, SSE boundaries, and attachment address validation.

## Live smoke tests

Live tests are opt-in, serialized, suppress response bodies on failure, and enforce a hard maximum of 30 upstream calls. The minimal suite makes six calls: streaming and non-streaming text through OpenAI, Anthropic, and Gemini.

```bash
cargo run --release --bin live-smoke -- --suite minimal
```

The full suite adds two-turn tools, a tiny data-URL image, and reasoning for each native provider. Anthropic additionally round-trips its opaque signed-thinking block through a continuation, totaling 19 calls:

```bash
cargo run --release --bin live-smoke -- --suite full --max-requests 20
```

The runner reads the existing Stackforward YAML directly into secret wrappers. It never prints, copies, snapshots, or writes the keys. Model IDs can be overridden with `HAARNESS_E2E_OPENAI_MODEL`, `HAARNESS_E2E_ANTHROPIC_MODEL`, and `HAARNESS_E2E_GEMINI_MODEL`.

To test a one-time Anthropic credential without changing the YAML or exposing it in process arguments, use the stdin-only override. Disable terminal echo before entering the key interactively:

```bash
stty -echo
cargo run --release --bin live-smoke -- \
  --only anthropic --anthropic-key-stdin --suite full --max-requests 8
stty echo
```

The override is deliberately rejected unless `--only anthropic` is also set.

For server-owned tools, model discovery, CLI continuation and real TUI
detach/reattach, run `scripts/test-haarness-attach.py` with the release binaries.
See [Haarness attach and current models](haarness.md#verification) for setup,
model selection and the known live Google project-access failure.

## Local load test

Terminal 1:

```bash
cargo run --release --bin mock-provider -- \
  --bind '[::1]:9092' \
  --hold-ms 620000
```

Terminal 2:

```bash
export HAARNESS_ROUTER_KEY='local-router-key'
export MOCK_PROVIDER_KEY='unused-local-provider-key'
cargo run --release --bin haarness-router -- --config tests/config.soak-split.toml
```

Terminal 3:

```bash
HAARNESS_ROUTER_KEY='local-router-key' \
cargo run --release --bin load-test -- \
  --endpoint http://127.0.0.1:8083/v1/chat/completions \
  --concurrency 10000 \
  --requests 10000 \
  --ramp-seconds 20 \
  --hold-seconds 600 \
  --p99-target-ms 10
```

The harness refuses non-loopback endpoints. It measures router-added first-chunk latency from a timestamp emitted inside the mock event and keeps all streams open for the ten-minute soak. The split IPv4/IPv6 configuration avoids exhausting one macOS loopback ephemeral-port pool with the 20,000 client/router and router/provider TCP legs. Measure router peak RSS alongside it with the operating system's process monitor; acceptance is at most 2 GiB with no monotonic post-warm-up growth.

`cargo audit` currently reports the explicitly acknowledged maintenance advisory `RUSTSEC-2026-0192` through the local PDF extraction chain. It is not a vulnerability and has no safe upgrade; `deny.toml` records the exception so it remains visible and reviewable.
