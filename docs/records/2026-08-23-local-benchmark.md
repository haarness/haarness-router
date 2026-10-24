# Local benchmark result

Measured on 2026-08-23 on the development macOS host with release binaries and local mock providers only.

## Ten-minute soak

- Requests and concurrency: 10,000 SSE streams.
- Hold: 600 seconds with 15-second upstream SSE heartbeats.
- Router errors: 0.
- Router-added first-chunk p99: 4.029 ms.
- Sampled steady-state router RSS: approximately 274–379 MiB.
- Post-disconnect allocator retention: approximately 601 MiB, below the 2 GiB ceiling.
- Connection cleanup: all 10,000 active streams returned to zero.

The macOS harness splits its two outbound TCP legs across IPv4 and IPv6 loopback. This prevents a single ephemeral-port pool from limiting a same-host 10,000-stream test.

## Final-binary cancellation pass

After the final bounded metrics and custom-auth changes, a second release-mode run sent 10,000 requests through the same timestamped mock:

- Router errors: 0.
- Router-added first-chunk p99: 0.452 ms.
- Cancellation accounting: 10,000/10,000 streams released and recorded as cancelled after clients intentionally stopped reading after the first chunk.

These numbers are local engineering evidence, not a capacity promise for a different kernel, TLS path, provider, proxy, or deployment topology.
