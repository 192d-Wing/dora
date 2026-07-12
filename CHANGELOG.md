# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
  `concat`, `hexstring`, equality); matched-class options are merged into
  responses below explicitly-configured options.

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
