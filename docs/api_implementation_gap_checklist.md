# Dora Management API Implementation Gap Checklist

This checklist tracks the gap between the target OAS 3.1 API in
`docs/openapi.yaml` and the current dora HTTP API implementation.

**Status as of branch `api-public-endpoints`.** `[x]` = done, `[ ]` = not done;
items annotated `— partial:` are started but incomplete.

## Contract And Serving

- [x] Add `docs/openapi.yaml` validation in CI. — `openapi-spec-validator` job.
- [x] Serve the OpenAPI document as unauthenticated JSON at `GET /openapi.json`.
- [x] Ensure every API response uses `application/json`. — the v1 API is JSON; `/metrics`/`/metrics-text` intentionally keep the Prometheus wire formats.
- [ ] Remove or intentionally replace legacy unversioned routes as part of the breaking API cleanup. — partial: `/config` → `/v1/config` and `/ping` removed; `/metrics`/`/metrics-text` kept (authenticated) as a Prometheus scrape surface; old combined `/v1/leases` still present.
- [x] Generate and return `X-Request-ID` on every response. — on success and error responses.
- [x] Include the generated `request_id` in all error bodies.

## Authentication

- [x] Add Bearer token authentication for sensitive endpoints. — via `DORA_API_TOKEN`.
- [x] Add mTLS authentication support, either in-process or through documented proxy integration. — in-process rustls TLS termination with optional client-cert (mTLS) verification against hot-reloaded trust anchors; server cert/key + client-CA are files (from external ACME/TAMP), polled and hot-swapped on rotation.
- [x] Accept either Bearer token or valid mTLS client certificate. — a verified client cert satisfies auth on its own; otherwise the Bearer token is required. The TLS layer stamps a trusted (unspoofable) marker header the `authorize` check reads.
- [ ] Keep only `GET /health`, `GET /ready`, and `GET /openapi.json` public. — partial: the metrics endpoints are also public.
- [x] Add authorization tests for public, authenticated, and rejected requests. — bearer accept/reject, mTLS accept, mTLS-absent-falls-back-to-bearer, and spoofed-marker rejection (TLS and plaintext).

## Health And Server Metadata

- [x] Replace current health body/status behavior with JSON `GET /health`.
- [x] Add `GET /ready` with structured readiness checks.
- [x] Add `GET /v1/server` with runtime metadata and server mode.
- [x] Track server modes: `normal`, `maintenance`, `drain`, and `shutting_down`. — a shared `SharedMode` handle (`dora-core`) is set by the maintenance-mode/drain/shutdown actions, reported by `/v1/server`, and enforced in the DHCP datapath (drain/shutting_down suppress new leases; maintenance also suppresses renewals).

## Metrics

- [x] Convert current Prometheus endpoints into JSON API endpoints.
- [x] Add `GET /v1/metrics/summary`.
- [x] Add `GET /v1/metrics` for detailed structured dora metrics.
- [x] Add `GET /v1/metrics/prometheus` using the OpenMetrics-inspired JSON shape.
- [ ] Decide whether old `/metrics` and `/metrics-text` are removed immediately or left behind a compatibility flag. — currently kept; decision pending.

## Leases

- [x] Replace current `GET /v1/leases` with separate `GET /v1/leases/v4` and `GET /v1/leases/v6`. — note: the old combined `/v1/leases` is still registered.
- [x] Add pagination with `limit` and `offset`.
- [x] Add response metadata: `limit`, `offset`, `total`, `count`, `filters`, and `sort`.
- [x] Add broad filters: `state`, `network`, `ip`, `client_id`, `expires_from`, and `expires_to`.
- [x] Add flexible sorting such as `sort=state,-expires_at,ip`. — multi-field, `-` for descending.
- [x] Implement DHCPv6 lease listing, including IA_NA and IA_PD where available.

## Reservations

- [x] Add runtime reservation storage. — DB-backed `runtime_reservations` table + an in-memory `RuntimeReservations` store (config crate) warmed on startup and read by the datapath.
- [x] Preserve config reservations. — surfaced via `GET /v1/reservations/v4` with `source: config`.
- [x] Define and enforce precedence: runtime API reservations override config reservations. — v4 `StaticAddr` checks the runtime store (MAC then option) before config; v6 `LeasesV6` pins the reserved IA_NA/IA_PD for a matching DUID before pool allocation. Runtime entries shadow same-address config entries in the listing.
- [x] Add `GET /v1/reservations/v4`. — config + runtime reservations, pagination + network/ip/client_id filters.
- [x] Add `GET /v1/reservations/v6`. — runtime v6 reservations (config has none).
- [x] Add action endpoints for create, update, and delete reservation. — `POST /v1/actions/{create,update,delete}-reservation`, sync (`200`) or async (`202`), each recording an audit operation.
- [x] Add conflict and precedence tests. — duplicate-address / duplicate-match conflicts (`409`); v4 and v6 datapath precedence tests (runtime overrides config / pool).

## Configuration Management

- [x] Change `/v1/config` to return structured redacted JSON, not YAML-as-string.
- [x] Keep secret redaction guarantees for DDNS TSIG data.
- [ ] Add full config management: read, validate, update, activate, reload, and rollback. — only read is implemented.
- [ ] Add versioned staged config candidates.
- [ ] Add candidate validation results.
- [ ] Add rollback-capable activation history.
- [ ] Ensure config writes are atomic.
- [ ] Define file locking or single-writer behavior for concurrent config updates.

## Automation Actions

- [x] Add action-oriented endpoints under `/v1/actions`. — partial: `maintenance-mode`, `drain`, `shutdown`, and `{create,update,delete}-reservation` implemented; config/lease/DDNS actions pending in later phases.
- [ ] Implement reload config.
- [ ] Implement activate config.
- [ ] Implement rollback config.
- [ ] Implement release lease.
- [ ] Allow per-request DDNS cleanup on lease release.
- [ ] Implement trigger DDNS update and cleanup.
- [x] Implement create/update/delete reservation actions. — `POST /v1/actions/{create,update,delete}-reservation`, enforced in the v4 + v6 datapath.
- [x] Implement maintenance mode. — `POST /v1/actions/maintenance-mode` (sync); suppresses new leases and renewals.
- [x] Implement drain mode. — `POST /v1/actions/drain` (sync); suppresses new leases, keeps renewals.
- [x] Implement graceful shutdown. — `POST /v1/actions/shutdown` (async `202`); enters shutting-down mode, cancels the shared token after the grace period.

## Async Operations And Audit

- [x] Add mixed sync/async action execution. — maintenance-mode/drain return `ActionResult` synchronously; shutdown returns `202 OperationAccepted` and completes out of band.
- [x] Add `GET /v1/operations/{operation_id}`.
- [x] Persist completed operation records until normal log/audit retention removes them. — DB-backed `operations` table.
- [x] Record actor/auth context, action input summary, status, timestamps, and errors.
- [x] Add tests for operation lifecycle: accepted, running, succeeded, failed, and canceled. — partial: accepted/running/succeeded exercised via the shutdown flow; failed/canceled paths land with the config/reservation actions that can fail.

## Error Model

- [x] Replace current `{ "message": "..." }` error bodies with `{ "error": { "code", "message", "request_id", "details" } }`.
- [x] Define stable machine-readable error codes. — `unauthorized`, `internal`; the envelope carries a `code` per error.
- [ ] Ensure validation errors include useful field/path details. — partial: the envelope has an optional `details` field, but no endpoint populates it yet.
- [x] Ensure internal errors do not leak secrets or filesystem internals. — 5xx return a generic message; the full error is logged server-side only.

## Documentation

- [x] Update README to remove stale `0.0.0.0:3333` external API default. — now documents `127.0.0.1:3333` and the v1 endpoint surface.
- [x] Update `crates/bin/README.md` to show `127.0.0.1:3333`. — and fixed the `--external-api` help text (was mislabeled "the v6 address").
- [ ] Document the breaking cleanup from legacy routes to the OAS v1 API.
- [ ] Document auth deployment patterns for Bearer token and mTLS.
- [ ] Document operational risk for config, reservations, drain, and shutdown actions.
