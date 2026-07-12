# RFC 8415 — Stateful DHCPv6 Implementation Plan

Status: **implemented** (Phases 1–6). IA_NA address assignment and IA_PD prefix
delegation with the full lifecycle and Rapid Commit are in `plugins/leases-v6`
and the supporting `config` / `ip-manager` crates.

| Phase | Scope | Status |
| --- | --- | --- |
| 1 | v6 config pools (`ranges`, `pd_pools`) | ✅ done |
| 2 | v6 lease storage (`leases_v6` table) | ✅ done |
| 3 | IA_NA Solicit/Advertise + Request/Reply + Rapid Commit | ✅ done |
| 4 | IA_NA lifecycle (Renew/Rebind/Confirm/Release/Decline) + v6 DAD | ✅ done |
| 5 | IA_PD prefix delegation | ✅ done |
| 6 | metrics, `/v1/leases` v6, examples, docs | ✅ done |

**Remaining follow-ups** (tracked in [rfc_compliance.md](./rfc_compliance.md)):
end-to-end packet-level v6 integration tests (need the netns/veth harness),
Neighbor-Solicitation DAD (currently ICMPv6 echo), relay (RelayForw/RelayRepl),
IA_TA, and Reconfigure.

This document originally planned the work; the design notes below are retained
for reference. It is the roadmap referenced by
[rfc_compliance.md](./rfc_compliance.md).

## Goal

Bring the DHCPv6 side from **stateless-only** (Information-Request / Reply, RFC
3736) to a conformant **stateful** server per
[RFC 8415](https://datatracker.ietf.org/doc/html/rfc8415):

- **IA_NA** (option 3) — non-temporary address assignment.
- **IA_PD** (option 25) — prefix delegation (RFC 8415 §6.3).
- Full exchange: Solicit/Advertise, Request/Reply, Renew, Rebind, Confirm,
  Release, Decline, plus **Rapid Commit** (option 14).

### Explicitly out of scope (for this effort)

- **Reconfigure** (server-initiated, option 19 / Reconfigure message) — requires
  Reconfigure Key auth; defer.
- **IA_TA** (temporary addresses, option 4) — rarely used; defer.
- **Authentication** (RFC 8415 §20), **LEASEQUERY**, **anycast**.
- **v6 DDNS** — tracked separately from this plan.

## Current state (starting point)

> **Note (codebase moved):** crates now live under `crates/` and the DHCP
> library is the `usg-dhcproto` fork (0.17.1), package-renamed to `dhcproto` so
> code still uses `dhcproto::`. Paths below reflect the `crates/` layout.

| Component | Today | File |
| --- | --- | --- |
| v6 server + plugin chain | Boots; registers only `MsgType` | [crates/bins/v6-server/src/main.rs](../crates/bins/v6-server/src/main.rs) |
| v6 message handling | `InformationRequest` only; stateful → `NoResponse` | [crates/plugins/message-type/src/lib.rs:445-467](../crates/plugins/message-type/src/lib.rs#L445-L467) |
| v6 config | Interfaces, DUID/server-id, per-network options, valid/preferred times | [crates/libs/config/src/v6.rs](../crates/libs/config/src/v6.rs) |
| v6 pools | **Missing** — `wire::v6::IpRange` defined but never wired into `Network` | [crates/libs/config/src/wire/v6.rs:112-121](../crates/libs/config/src/wire/v6.rs#L112-L121) |
| IP storage | `IpAddr` interface, but v4-bound: `NetRange`, `HashSet<Ipv4Addr>`, `Icmpv4`, `ip INTEGER` | [crates/libs/ip-manager/src/lib.rs](../crates/libs/ip-manager/src/lib.rs) |
| Lease schema | One row per IP, `ip INTEGER`, keyed by `ip` — **128-bit v6 addr does not fit** | [migrations/20210824204854_initial.sql](../migrations/20210824204854_initial.sql) |

## Key design decisions

### 1. Identity & binding model

DHCPv6 identifies clients by **DUID**, and each client requests one or more
**IAs** identified by an **IAID**. Each IA (IA_NA or IA_PD) may hold multiple
addresses/prefixes.

```text
Client (DUID)
 └── IA_NA (IAID) ──> [address, address, ...]
 └── IA_PD  (IAID) ──> [prefix/len, ...]
```

Bindings are therefore keyed on **(DUID, IAID, resource)** where `resource` is an
address (IA_NA) or a prefix+length (IA_PD). This differs fundamentally from the
v4 model (one row per IP, identity = chaddr/opt-61).

### 2. Storage: new `leases_v6` table (do not widen the v4 table)

The v4 `leases` table uses `ip INTEGER` and cannot hold a 128-bit address. Rather
than widen it and disturb the hot v4 path, add a **separate table**:

```sql
CREATE TABLE IF NOT EXISTS leases_v6(
    addr        BLOB    NOT NULL,   -- 16-byte address or prefix base
    prefix_len  INTEGER NOT NULL,   -- 128 for IA_NA, delegated length for IA_PD
    duid        BLOB    NOT NULL,
    iaid        INTEGER NOT NULL,
    ia_type     INTEGER NOT NULL,   -- IA_NA vs IA_PD
    network     BLOB    NOT NULL,
    preferred_at INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,   -- valid lifetime
    state       INTEGER NOT NULL,   -- lease / probate / reserve
    PRIMARY KEY(addr, prefix_len)
);
CREATE INDEX idx_v6_duid ON leases_v6 (duid, iaid);
```

`Storage` gains a v6-facing set of methods (insert/get/release/next-available by
DUID+IAID and by pool range). Keep the existing v4 methods unchanged.

### 3. Unify the allocation layer up front (decided)

`IpManager::reserve_first` is generic in name but concrete in v4 types
([crates/libs/ip-manager/src/lib.rs:282-289](../crates/libs/ip-manager/src/lib.rs#L282-L289)):
`config::v4::NetRange`, `HashSet<Ipv4Addr>` exclusions, `Icmpv4`.

**Decision: generalize the `IpManager` / `Storage` allocation layer to handle
both v4 and v6 from the start**, rather than a throwaway parallel v6 path. The
allocation core ("next-expired / max-in-range") becomes address-family-agnostic
(operating on `IpAddr` + a family-neutral pool/exclusion abstraction), with v4
and v6 config range types feeding a common interface. This is more work in
Phase 2/3 but avoids building — then reworking — a second path. The v4 hot path
must remain behaviorally unchanged (regression-tested).

### 3a. v6 DAD is in scope (decided)

Duplicate Address Detection for v6 will be implemented (not deferred). v6 DAD is
**Neighbor Solicitation** based, not ICMP echo like v4, so this adds an `Icmpv6`
/ NS path to `libs/icmp-ping` (or a sibling) and generalizes `IpManager`'s
ping/DAD hook over address family. `ping_check` will be honored for v6 networks.

### 4. Plugin structure

Add a **`leases-v6` plugin** registered on the v6 `Server` alongside `MsgType`.
`MsgType`'s v6 handler is extended to build the base Reply/Advertise skeleton and
classify the message; `leases-v6` performs IA_NA/IA_PD allocation. This mirrors
the v4 `MsgType` → `Leases` split.

## Phases

Each phase is independently reviewable and leaves the tree building & green.

### Phase 1 — Config: v6 address & prefix pools

- Wire `ranges` into the parsed v6 `Network` (address pools), mirroring v4
  `NetRange`: start/end, per-range options, per-range valid/preferred, exclusions.
- Add `pd_pools` (prefix + delegated length) to the v6 `Network` for IA_PD.
- Sample configs (`config_v6.yaml`) with pools; parser unit tests.
- **No runtime behavior change** — parsing only.

### Phase 2 — Storage: v6 binding persistence

- New migration: `leases_v6` (schema above).
- Extend `Storage` trait with v6 methods; implement in `sqlite.rs`.
- Binding model helpers (DUID+IAID ↔ addresses/prefixes); unit tests against
  `sqlite::memory:`.

### Phase 3 — IA_NA: SOLICIT/ADVERTISE + REQUEST/REPLY + Rapid Commit

- New `leases-v6` plugin; register on v6 server in `main.rs`.
- Extend `MsgType` v6 to classify Solicit/Request and build the response skeleton.
- **Solicit → Advertise**: allocate/offer an address, emit IA_NA + IAADDR,
  Preference (option 7), Status Code (option 13, e.g. `NoAddrsAvail`).
- **Request → Reply**: commit the binding; set T1/T2 and preferred/valid
  lifetimes per RFC 8415 §14 / §21.4.
- **Rapid Commit** (option 14): Solicit → Reply two-message exchange.
- Server-id / client-id validation per §16; ORO-driven option population
  (reuse `MsgContext::<v6>::populate_opts`).

### Phase 4 — IA_NA lifecycle: RENEW / REBIND / CONFIRM / RELEASE / DECLINE

- **Renew** (unicast to server) / **Rebind** (any server): extend binding,
  return `NoBinding` status when unknown (§18.3.4/.5).
- **Confirm**: on-link check for the client's addresses (§18.3.3).
- **Release**: mark binding released; **Decline**: probation (reuse v4 concept).

### Phase 5 — IA_PD prefix delegation (§6.3, §18)

- Prefix allocation from `pd_pools`; IA_PD (25) + IAPREFIX (26) in responses.
- Renew/Rebind/Release/Confirm for delegated prefixes.
- Storage already carries `prefix_len` from Phase 2.

### Phase 6 — Integration, metrics, docs

- v6 integration-test harness mirroring [crates/integration-tests/tests](../crates/integration-tests/tests)
  (Solicit→Reply, Renew, Release, Rapid Commit, IA_PD).
- Wire the already-defined v6 metrics counters
  ([crates/dora-core/src/server/context.rs:773-846](../crates/dora-core/src/server/context.rs#L773-L846)).
- `example.yaml` v6 pool/pd examples; update
  [rfc_compliance.md](./rfc_compliance.md) status rows.

## RFC 8415 option coverage checklist

| Option | Code | Phase |
| --- | --- | --- |
| Client Identifier (DUID) | 1 | 3 |
| Server Identifier (DUID) | 2 | 3 (exists) |
| IA_NA | 3 | 3 |
| IA Address (IAADDR) | 5 | 3 |
| Option Request (ORO) | 6 | 3 (exists) |
| Preference | 7 | 3 |
| Elapsed Time | 8 | 3 |
| Status Code | 13 | 3 |
| Rapid Commit | 14 | 3 |
| IA_PD | 25 | 5 |
| IA Prefix (IAPREFIX) | 26 | 5 |

## Risks & open questions

- **Unified allocation refactor** (decision #3, decided) — v4 hot path must stay
  behaviorally unchanged; guard with regression tests before/after.
- **v6 DAD** (decided in scope) — Neighbor Solicitation differs from v4 ICMP
  echo; adds an `Icmpv6`/NS path and generalizes the DAD hook over family.
- **`usg-dhcproto` 0.17.1** v6 API surface **confirmed**: `IANA{id,t1,t2,opts}`
  and `IAPD{id,t1,t2,opts}` carry nested `IAAddr{addr,preferred_life,valid_life,opts}`
  / `IAPrefix{prefix_len,prefix_ip,preferred_lifetime,valid_lifetime,opts}` inside
  `opts`; `StatusCode{status,msg}` with `Status` consts (`NoAddrsAvail`,
  `NoBinding`, `NotOnLink`, `NoPrefixAvail`, `UseMulticast`, …); `RapidCommit`
  is a unit variant.
- **Rebind server scope** — confirm desired behavior when multiple `dora`
  instances serve the same link (HA is a separate roadmap item).

## Test strategy

- Unit tests per phase (config parsing, storage, allocation).
- Integration tests driving real Solicit/Request/Renew/Release packets through
  the v6 server, asserting on decoded IA_NA/IA_PD in replies.
- `dhcpm` / `perfdhcp -6` for manual/soak verification.
