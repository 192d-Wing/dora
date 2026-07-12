# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/192d-Wing/dora/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/192d-Wing/dora/compare/v0.2.0...v0.3.0
