# dora management API

dora serves a JSON management/observability API (OpenAPI 3.1 contract:
[`docs/openapi.yaml`](./openapi.yaml), also served unauthenticated at
`GET /openapi.json`). It binds to `127.0.0.1:3333` by default
(`--external-api` / `EXTERNAL_API`).

## Authentication

Two mechanisms, and a request is authorized if **either** succeeds:

- **Bearer token** — set `DORA_API_TOKEN`; clients send
  `Authorization: Bearer <token>`. The compare is constant-time.
- **mTLS client certificate** — when TLS is enabled with a client-CA bundle, a
  request presenting a certificate that verifies against the trust anchors is
  authorized without a bearer token.

Public routes (no auth): `GET /health`, `GET /ready`, `GET /openapi.json`, and
`GET /docs` (interactive Swagger UI + its assets).

### Interactive docs

`GET /docs` serves a [Swagger UI](https://github.com/swagger-api/swagger-ui)
page rendering the live contract from `GET /openapi.json`. The assets are
vendored into the binary (no CDN), so the page works in an air-gapped
deployment. Use the **Authorize** button to supply a Bearer token before trying
authenticated endpoints from the browser.

### TLS / mTLS deployment

dora terminates TLS in-process (intended to sit behind a TLS-passthrough load
balancer, e.g. Cilium). Certificates are files on disk that dora **hot-reloads**
on rotation — dora does not speak ACME or manage trust anchors itself; an
external ACME client provides the server cert/key and an external TAMP client
provides the client-CA bundle.

| Option (`--flag` / `ENV`) | Purpose |
| --- | --- |
| `--external-api-tls-cert` / `EXTERNAL_API_TLS_CERT` | server certificate chain (PEM) |
| `--external-api-tls-key` / `EXTERNAL_API_TLS_KEY` | server private key (PEM) |
| `--external-api-tls-client-ca` / `EXTERNAL_API_TLS_CLIENT_CA` | client-cert trust anchors (PEM); enables mTLS |
| `--external-api-tls-reload-secs` / `EXTERNAL_API_TLS_RELOAD_SECS` | how often to re-read the files (default 30) |

- TLS is enabled when **both** cert and key are set; otherwise the API serves
  plaintext (the default for local/dev, intended behind a terminating proxy).
- **Optional vs mandatory mTLS.** With a client-CA **and** a bearer token
  configured, mTLS is *optional* — a client may present a cert or use the bearer.
  With a client-CA and **no** bearer token, mTLS is *mandatory* at the TLS layer
  (a certless client can't connect), so a `client-ca`-only deployment can never
  be left open to unauthenticated clients.

Protected routes fail closed when neither method is configured. For trusted local
development only, set `DORA_API_ALLOW_UNAUTHENTICATED=true` to explicitly opt out.
- The config-lifecycle write endpoints (below) additionally **require** a
  verified client certificate whenever a client-CA is configured — this is how a
  GitOps orchestrator's certificate gates configuration pushes in production.

Example (production, GitOps orchestrator pushes config over mTLS):

```
dora \
  --external-api 0.0.0.0:3333 \
  --external-api-tls-cert /etc/dora/tls/server.pem \
  --external-api-tls-key  /etc/dora/tls/server.key \
  --external-api-tls-client-ca /etc/dora/tls/trust-anchors.pem
```

## Route surface

The API is versioned under `/v1`. Legacy unversioned routes have been replaced:

- `GET /config` → `GET /v1/config` (structured, redacted JSON — no longer
  YAML-as-a-string).
- The combined `GET /v1/leases` is superseded by `GET /v1/leases/v4` and
  `GET /v1/leases/v6` (with pagination, filters, and sorting).
- `GET /metrics` / `GET /metrics-text` (Prometheus text) remain for scrapers, but
  the structured equivalents live at `GET /v1/metrics`,
  `GET /v1/metrics/summary`, and `GET /v1/metrics/prometheus`.
- `/ping` was removed (use `GET /health` / `GET /ready`).

## Operational risk

Some actions change server behavior or state — treat them as privileged
operations, and prefer running the API over mTLS in production.

- **`activate-config` / `rollback-config` / `reload`** (config lifecycle): write
  the config file atomically and then **gracefully restart** dora so the datapath
  adopts the new config. In-flight DHCP work is drained (bounded), but the
  process exits and relies on a supervisor (systemd/Kubernetes) to bring it back.
  Candidate validation is parse-level (matches the on-disk loader) and does not
  perform a full dry-run boot, so a config that fails at interface/socket bind
  would surface on the restart. These endpoints require mTLS when a client-CA is
  configured.
- **`drain` / `maintenance-mode`**: suppress new leases (and, for maintenance,
  renewals) in the DHCP datapath — clients won't get addresses while active.
- **`shutdown`**: begins a graceful shutdown; the process exits after the grace
  period.
- **`{create,update,delete}-reservation`**: change which addresses clients
  receive (runtime reservations override config reservations and the pool).
- **`release-lease`**: frees a lease immediately; the address may be handed to
  another client. `ddns_cleanup` also removes the reverse DNS record.
- **`trigger-ddns-update`**: performs DNS updates/deletes against the configured
  DDNS servers (v4).

Mutating actions record an audit row retrievable via
`GET /v1/operations/{operation_id}` (actor, input summary, status, timestamps).
