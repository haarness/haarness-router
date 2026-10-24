# Observability

The router reports through three channels: health endpoints, Prometheus metrics and JSON logs. None of
them carries prompts, attachments, tool arguments, upstream bodies, credentials or the raw model string a
client sent; label values come only from the configuration.

## Health

`GET /healthz` and `GET /readyz` are always unauthenticated and answer `200 {"status":"ok"}` while the
process serves. They do not call providers: a healthy router can still have no working upstream, which the
metrics and the dashboard's **Overview** show.

## Metrics

`GET /metrics` returns the Prometheus text format. It follows the model API's authentication: open when
authentication is off, otherwise it needs a router key or the scrape-only token named by
`auth.metrics_token_env` (see [configuration](configuration.md#authentication-and-metrics)).

| Metric | Type | Labels |
| --- | --- | --- |
| `haarness_router_requests_total` | counter | — |
| `haarness_router_errors_total` | counter | — |
| `haarness_router_active_streams` | gauge | — |
| `haarness_router_completed_streams_total` | counter | — |
| `haarness_router_failed_streams_total` | counter | — |
| `haarness_router_cancelled_streams_total` | counter | — |
| `haarness_router_response_bytes_total` | counter | — |
| `haarness_router_fallback_attempts_total` | counter | — |
| `haarness_router_first_event_microseconds_total` | counter | — |
| `haarness_router_completion_microseconds_total` | counter | — |
| `haarness_router_route_<name>` | as above | `provider`, `model` |
| `haarness_router_upstream_failures_total` | counter | `provider`, `model`, `alias`, `class` |
| `haarness_router_circuit_state` | gauge | `provider`, `model` |

- `haarness_router_route_*` repeats the request, stream, byte, fallback and timing series per configured
  provider and model: `requests_total`, `active_streams`, `completed_streams_total`,
  `failed_streams_total`, `cancelled_streams_total`, `response_bytes_total`, `fallback_attempts_total`,
  `first_event_microseconds_total` and `completion_microseconds_total`.
- `model` is a declared model, or `_undeclared` for an exact `provider/model` that is not under
  `[models]`. `alias` is the configured alias, or `_direct` for an exact model.
- `haarness_router_upstream_failures_total` counts every failed upstream call, including fallback attempts
  a later target recovered, with `class` one of `4xx`, `5xx`, `timeout`, `connect` or `other`.
- `haarness_router_circuit_state` is `0` closed, `1` open, `2` half-open.
- Every series exists from startup, so a dashboard sees zeros rather than gaps.

A Prometheus job that reads the scrape token from a file:

```yaml
scrape_configs:
  - job_name: haarness-router
    authorization:
      type: Bearer
      credentials_file: /run/secrets/haarness-metrics-token
    static_configs:
      - targets: ["127.0.0.1:8080"]
```

## Logs

Logs are JSON lines on standard output. `RUST_LOG` replaces the default `info,tower_http=warn` filter.

At `info`, each chat completion request writes one `request finished` line with: the request ID, the
caller's W3C trace ID when a valid `traceparent` arrived, alias (`_direct` for an exact model), provider,
model label (declared or `_undeclared`), HTTP status, outcome (`completed`, `failed`, `cancelled`, or
`rejected` before any upstream call), the router's error code, the fallback count, the provider's own
request ID and the duration. `debug` adds a `request routed` line with the same labels.

## Correlating a request

- Send `X-Request-ID` (1–128 characters of `[A-Za-z0-9._:-]`) or let the router generate
  `req_<UUIDv7>`; every response echoes it, and the log line records it.
- To follow a request into a provider, use the provider's own request ID in the log line (`x-request-id`
  from OpenAI-compatible APIs, `request-id` from Anthropic, `x-amzn-requestid` from Bedrock). The router
  never sends its request ID or trace context upstream; see the [security model](security.md).
- `X-Haarness-Provider`, `X-Haarness-Model` and `X-Haarness-Attempts` on the response say which target
  answered and after how many attempts.

The dashboard's **Observability** view parses the same metrics and shows the raw exposition; see
[Web UI](web-ui.md).
