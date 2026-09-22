# Security model

New self-hosted configurations have no router-key requirement. `auth.enabled`
explicitly selects the mode; an older configuration with `router_key_envs` and
no `enabled` value retains authentication. With authentication disabled, anyone
who can reach the model API can use it. Web UI access controls remain independent.

- Router credentials and provider credentials are different secret domains. The server hashes router keys with SHA-256 at startup and compares digests in constant time. Provider keys remain in `SecretString` containers.
- The embedded Web UI never receives provider secret values or secret environment-variable names. Its overview API exposes only whether a provider credential is configured. A caller-supplied upstream key is held only for its playground request and is never stored by the UI.
- Optional Web UI HTTP Basic credentials are loaded through environment-variable references and compared as SHA-256 digests in constant time. UI responses use `no-store`, `nosniff`, frame denial, referrer denial, and a restrictive Content Security Policy. CORS remains disabled.
- Router keys created in the Web UI are process-local and stored only as SHA-256 digests. Their plaintext is returned once, bootstrap keys cannot be revoked through the UI, state-changing administration calls require a same-origin marker, and all runtime keys disappear on restart.
- JSON logs include request ID, normalized status, timing, fallback count, configured provider, and a declared-model or `_undeclared` model label. Prometheus metrics use the same bounded route labels and expose active/completed/failed/cancelled streams, bytes, first-event/completion timing totals, fallback attempts, and circuit state. Headers, request bodies, prompts, attachments, tool arguments, upstream response bodies, and credentials are excluded.
- Configuration contains environment-variable names, not secret values. Missing referenced variables fail startup.
- `X-Upstream-API-Key` is disabled by default. When enabled it requires an exact model and disables alias fallback and configured-key fallback.
- Public attachment URLs must use HTTPS without embedded credentials. Local PDF materialization resolves DNS, rejects any private/loopback/link-local/multicast result, pins a validated public address, disables implicit redirects, and revalidates up to three redirects.
- PDF downloads are limited to 20 MiB and extracted text to 4 MiB. Request bodies default to 32 MiB and SSE frames to 1 MiB.
- HTTP provider URLs require explicit `allow_insecure_http = true` and are intended only for loopback tests.
- CORS is not enabled. The default bind address is loopback.
- A client disconnect drops the response guard, cancels the upstream token, and releases its concurrency permit.

Operational deployment should terminate TLS at a trusted reverse proxy, protect or isolate `/metrics`, enable Web UI Basic authentication (or stronger authentication at the proxy), rotate the UI/router/provider credentials independently, and avoid injecting secrets through command-line arguments or checked-in files. HTTP Basic is suitable only over loopback or HTTPS.
