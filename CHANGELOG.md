# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.4] - 2026-07-14

### Added

- **Forensic/legal logging plugin** — compliance-grade audit logging of every
  DHCP lease lifecycle event (v4 and v6). Events are emitted to a dedicated
  `forensic_log` tracing target so operators can route them independently to a
  separate file (daily rotation via `DORA_FORENSIC_LOG_PATH` env var), stdout
  for container log shippers, or both. Runs as a `PostResponse` plugin with zero
  latency impact on the DHCP response path. Gated by a `forensic_log:` config
  section with `enabled` (default true) and `format` (`json` or `text`).

## [0.7.3] - 2026-07-14

### Added

- DHCPv6 domain search list (option 24) and NTP server (option 56) option types
  in config.

### Fixed

- An address found in use by duplicate-address detection is now probated outside
  the ping cache, so the hold-out survives a cache eviction.
- Hardened lease, allocation, and classification paths (code review findings).

## [0.7.2] - 2026-07-14

### Security

- The DHCPv4 renew-cache fast path no longer hands a client an address it does
  not hold. Entries are keyed by client-id only and stored a bare remaining
  lease time, so a client within its renew window could send a SELECTING REQUEST
  for any in-pool address and be ACKed it. Cache entries are now bound to the
  granted address; the fast path fires only when the client re-requests that
  exact address.
- IA_PD prefix delegation is now claimed atomically (a single
  `INSERT … ON CONFLICT DO UPDATE … WHERE … RETURNING`). The previous
  check-then-write pair let two concurrent clients be delegated the same prefix.
- IA_NA/IA_PD options are capped per message, bounding the pool reservations, DB
  round-trips, and response size one relayed packet can induce.
- The `next_expired` client-id match is now range-bounded, so a client with a
  lease in another range/network can no longer have that binding matched,
  mutated, and released while serving an unrelated request.

### Fixed

- An address found in use by duplicate-address detection is now held out for the
  full probation period instead of being deleted (which held it out only for the
  ping-cache TTL).
- Released addresses are reclaimed instead of leaking: RELEASE now marks the row
  expired and unowned rather than deleting it, so the address is reused by
  `next_expired` instead of leaving a permanent hole below the allocator's
  high-water mark.
- Recycling a different client's expired lease now runs duplicate-address
  detection (never on the client's own address); an authoritative server
  reclaims an expired row on REQUEST instead of NAKing.
- The renew cache is cleared by client-id (not `chaddr`) on DECLINE.
- Client-classification expressions no longer panic on `split(x, d, 0)` or a
  `substring` length of `isize::MIN`, and a malformed relay sub-option is treated
  as absent rather than turning every relay lookup into an evaluation error.
- `determine_lease` tolerates a misconfigured `min > max` instead of panicking.
- Bumped `usg-dhcproto` to 0.19.1, hardening DHCPv6 option decoding against
  malformed options whose declared length is shorter than their fixed header
  (`StatusCode`, `VendorClass`, `VendorOpts`), reachable via the relay-message
  decode path.

## [0.7.1] - 2026-07-12

### Fixed

- DHCPv4 option 1 (subnet mask) is now derived from the network key's CIDR
  prefix (e.g. `10.1.0.0/24` → `255.255.255.0`) and included in OFFER/ACK
  responses for relayed packets. Previously the mask was only derived from the
  server's local interface, which is on a different subnet for relayed traffic,
  so option 1 was absent unless explicitly configured.
- The interface-derived default router is now dropped from the response when the
  CIDR-derived subnet mask would make it unreachable (e.g. interface on a /16
  but DHCP scope is a /24).

## [0.7.0] - 2026-07-12

### Fixed

- DHCPv4 option 125 (Vendor-Identifying Vendor-Specific Information) is now
  served verbatim. Bumped `usg-dhcproto` to 0.19.0, which stores option 125 as
  opaque bytes instead of parsing it as RFC 3925. Vendor framing that is not
  RFC-3925-shaped (e.g. TEO phones' 2-byte PEN + 1-byte length) previously
  failed to parse and was silently dropped, so the option never reached the
  client.

## [0.6.0] - 2026-07-12

### Changed

- Client-class options now take priority over range/network/policy/global
  options. Previously range options won; now matched client-class options
  override the same option codes from any config scope.

## [0.5.0] - 2026-07-12

### Added

- Type the config document in the management API (`GET/PUT /v1/config`, config
  candidates). The previously opaque `document` object is now the fully typed
  `DhcpConfig` schema in the OpenAPI spec, generated from `config_schema.json`,
  so DHCPv4/v6 **client classes, policies, and global options** (and networks,
  ranges, reservations, ddns, pd_pools, server_id) are first-class and
  discoverable via the API.
- DHCPv4 **runtime (API-managed) reservations** can now carry `options`,
  `class`, and `lease_time`, matching config reservations; a matched client
  receives the reservation's options. Stored in a new
  `runtime_reservations` migration and accepted/returned by the
  create/update/list-reservation endpoints. (v6 reservations remain
  address/prefix pins.)

## [0.4.0] - 2026-07-12

### Added

- DHCPv4/v6 **global options** (`v4.options` / `v6.options`) and named,
  reusable **policies** (`v4.policies` / `v6.policies`) referenced by a `policy`
  key on a network (and, for v4, on ranges/reservations). Precedence is
  most-specific-wins: range/reservation > network > policy > global.
- DHCPv6 **client classes** (`v6.client_classes`) supporting the
  protocol-agnostic expression subset (`option[code]`, `member`, `substring`,
  `concat`, `hexstring`, equality); matched-class options take priority
  over explicitly-configured options.

### Changed

- **DHCPv6 option precedence (behavior change):** when the same option code is
  set both globally (`v6.options`) and on a network (`networks.<subnet>.options`),
  the **network** value now wins. Previously the global value overrode the
  network value. Review any v6 config that sets the same code at both levels.

## [0.3.0] - 2026-07-12

### Security

- Harden the management API: protected routes now **fail closed**, requiring a
  bearer token or mTLS. Neither configured means requests are rejected; set
  `DORA_API_ALLOW_UNAUTHENTICATED=true` to explicitly opt out for trusted local
  development only.
- Upgrade `sqlx` 0.6 → 0.8, closing **RUSTSEC-2024-0363** (binary-protocol
  misinterpretation, patched in 0.8.1) and dropping the transitively vulnerable
  `ring` 0.16.20 it pulled in via `rustls` 0.20.
- Add `.cargo/audit.toml` recording the single reviewed `cargo audit` exception
  (RUSTSEC-2023-0071 `rsa`, which is not present in the resolved
  PostgreSQL-only build graph).

### Changed

- Bump the cargo dependency group (11 crates) and migrate the source to the new
  major APIs:
  - `syn` 1 → 2 — proc-macro `Register` derive (`register_derive_impl`)
  - `jsonschema` 0.16 → 0.47 — `dora-cfg` schema validation
  - `rcgen` 0.13 → 0.14 — `Issuer`-based certificate signing (test PKI)
  - `axum` 0.7 → 0.8 — `/{param}` route parameter syntax
  - plus `criterion`, `socket2`, `ring` 0.17, `phf`, `bytes`, and
    `crossbeam-channel`
- Replace the sqlx `sqlx-data.json` offline query cache with the `.sqlx/`
  directory produced by `cargo sqlx prepare`.
- Unify all workspace crates to a single version via `[workspace.package]`;
  every crate now sets `version.workspace = true` (0.3.0).

[Unreleased]: https://github.com/192d-Wing/dora/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/192d-Wing/dora/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/192d-Wing/dora/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/192d-Wing/dora/compare/v0.2.0...v0.3.0
