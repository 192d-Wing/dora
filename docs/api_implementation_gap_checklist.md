# Dora Management API Implementation Gap Checklist

This checklist tracks the gap between the target OAS 3.1 API in
`docs/openapi.yaml` and the current dora HTTP API implementation.

## Contract And Serving

- [ ] Add `docs/openapi.yaml` validation in CI.
- [ ] Serve the OpenAPI document as unauthenticated JSON at `GET /openapi.json`.
- [ ] Ensure every API response uses `application/json`.
- [ ] Remove or intentionally replace legacy unversioned routes as part of the breaking API cleanup.
- [ ] Generate and return `X-Request-ID` on every response.
- [ ] Include the generated `request_id` in all error bodies.

## Authentication

- [ ] Add Bearer token authentication for sensitive endpoints.
- [ ] Add mTLS authentication support, either in-process or through documented proxy integration.
- [ ] Accept either Bearer token or valid mTLS client certificate.
- [ ] Keep only `GET /health`, `GET /ready`, and `GET /openapi.json` public.
- [ ] Add authorization tests for public, authenticated, and rejected requests.

## Health And Server Metadata

- [ ] Replace current health body/status behavior with JSON `GET /health`.
- [ ] Add `GET /ready` with structured readiness checks.
- [ ] Add `GET /v1/server` with runtime metadata and server mode.
- [ ] Track server modes: `normal`, `maintenance`, `drain`, and `shutting_down`.

## Metrics

- [ ] Convert current Prometheus endpoints into JSON API endpoints.
- [ ] Add `GET /v1/metrics/summary`.
- [ ] Add `GET /v1/metrics` for detailed structured dora metrics.
- [ ] Add `GET /v1/metrics/prometheus` using the OpenMetrics-inspired JSON shape.
- [ ] Decide whether old `/metrics` and `/metrics-text` are removed immediately or left behind a compatibility flag.

## Leases

- [ ] Replace current `GET /v1/leases` with separate `GET /v1/leases/v4` and `GET /v1/leases/v6`.
- [ ] Add pagination with `limit` and `offset`.
- [ ] Add response metadata: `limit`, `offset`, `total`, `count`, `filters`, and `sort`.
- [ ] Add broad filters: `state`, `network`, `ip`, `client_id`, `expires_from`, and `expires_to`.
- [ ] Add flexible sorting such as `sort=state,-expires_at,ip`.
- [ ] Implement DHCPv6 lease listing, including IA_NA and IA_PD where available.

## Reservations

- [ ] Add runtime reservation storage.
- [ ] Preserve config reservations.
- [ ] Define and enforce precedence: runtime API reservations override config reservations.
- [ ] Add `GET /v1/reservations/v4`.
- [ ] Add `GET /v1/reservations/v6`.
- [ ] Add action endpoints for create, update, and delete reservation.
- [ ] Add conflict and precedence tests.

## Configuration Management

- [ ] Change `/v1/config` to return structured redacted JSON, not YAML-as-string.
- [ ] Keep secret redaction guarantees for DDNS TSIG data.
- [ ] Add full config management: read, validate, update, activate, reload, and rollback.
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

- [ ] Replace current `{ "message": "..." }` error bodies with `{ "error": { "code", "message", "request_id", "details" } }`.
- [ ] Define stable machine-readable error codes.
- [ ] Ensure validation errors include useful field/path details.
- [ ] Ensure internal errors do not leak secrets or filesystem internals.

## Documentation

- [ ] Update README to remove stale `0.0.0.0:3333` external API default.
- [ ] Update `crates/bin/README.md` to show `127.0.0.1:3333`.
- [ ] Document the breaking cleanup from legacy routes to the OAS v1 API.
- [ ] Document auth deployment patterns for Bearer token and mTLS.
- [ ] Document operational risk for config, reservations, drain, and shutdown actions.
