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
- [ ] Add mTLS authentication support, either in-process or through documented proxy integration.
- [ ] Accept either Bearer token or valid mTLS client certificate. — Bearer only.
- [ ] Keep only `GET /health`, `GET /ready`, and `GET /openapi.json` public. — partial: `/ping` and the metrics endpoints are also public.
- [ ] Add authorization tests for public, authenticated, and rejected requests. — partial: rejected (401) cases covered; full matrix not.

## Health And Server Metadata

- [x] Replace current health body/status behavior with JSON `GET /health`.
- [x] Add `GET /ready` with structured readiness checks.
- [x] Add `GET /v1/server` with runtime metadata and server mode.
- [ ] Track server modes: `normal`, `maintenance`, `drain`, and `shutting_down`. — partial: `ServerMode` enum exists but only `Normal` is ever reported; no transitions.

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

- [ ] Add runtime reservation storage.
- [x] Preserve config reservations. — surfaced via `GET /v1/reservations/v4` with `source: config`.
- [ ] Define and enforce precedence: runtime API reservations override config reservations.
- [x] Add `GET /v1/reservations/v4`. — config reservations, pagination + network/ip/client_id filters.
- [x] Add `GET /v1/reservations/v6`. — empty until runtime v6 reservations exist.
- [ ] Add action endpoints for create, update, and delete reservation.
- [ ] Add conflict and precedence tests.

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

- [ ] Add action-oriented endpoints under `/v1/actions`.
- [ ] Implement reload config.
- [ ] Implement activate config.
- [ ] Implement rollback config.
- [ ] Implement release lease.
- [ ] Allow per-request DDNS cleanup on lease release.
- [ ] Implement trigger DDNS update and cleanup.
- [ ] Implement create/update/delete reservation actions.
- [ ] Implement maintenance mode.
- [ ] Implement drain mode.
- [ ] Implement graceful shutdown.

## Async Operations And Audit

- [ ] Add mixed sync/async action execution.
- [ ] Add `GET /v1/operations/{operation_id}`.
- [ ] Persist completed operation records until normal log/audit retention removes them.
- [ ] Record actor/auth context, action input summary, status, timestamps, and errors.
- [ ] Add tests for operation lifecycle: accepted, running, succeeded, failed, and canceled.

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
