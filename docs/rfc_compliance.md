# RFC Compliance

This document tracks `dora`'s conformance to the DHCP RFCs: what is implemented,
what is partial, and what is missing. It is a living document — please open an
issue or PR if you find a discrepancy between this document and the code, or
between the code and an RFC.

Legend:

- ✅ **Implemented** — behavior is present and believed conformant.
- 🟡 **Partial** — implemented with known deviations or simplifications.
- ❌ **Missing** — not implemented.

---

## Summary

`dora` is a mature, largely conformant **DHCPv4** server (RFC 2131 / 2132) with
good coverage of the relay, subnet-selection, and DDNS extension RFCs, plus
**stateful DHCPv6** (RFC 8415: IA_NA + IA_PD, see `plugins/leases-v6`). The main
remaining gaps, in order of impact, are:

1. **The DHCPREQUEST client-state machine (RFC 2131 §4.3.2) is collapsed** — the
   four request states (SELECTING / INIT-REBOOT / RENEWING / REBINDING) are
   handled by a single path.
2. **DHCPv6 IA_TA and Reconfigure** are not implemented; v6 DAD uses ICMPv6 echo
   rather than Neighbor-Solicitation. (Relay — RelayForw/RelayRepl — is now
   supported.)
3. **Several §4.1 / §4.3 edge behaviors are simplified** — notably DHCPINFORM
   gating and the broadcast flag on replies to DHCPREQUEST.

---

## DHCPv4 core — RFC 2131

| Feature | Status | Notes |
| --- | --- | --- |
| DHCPDISCOVER → DHCPOFFER | ✅ | `plugins/message-type`, `plugins/leases` |
| DHCPREQUEST → DHCPACK/DHCPNAK | 🟡 | Works, but request states are not differentiated — see below |
| DHCPDECLINE (address probation) | ✅ | `Leases::decline`, IP put on probation for `probation_period` |
| DHCPRELEASE (no reply) | ✅ | `Leases::release` returns `NoResponse` |
| DHCPINFORM | 🟡 | Gated on `authoritative` + a matching range — see below |
| Rapid Commit (option 80, RFC 4039) | ✅ | `rapid_commit` config flag; DISCOVER answered with ACK |
| BOOTP (RFC 951 / 1542) | 🟡 | Supported when `bootp_enabled`; 300-byte min packet padding **not** implemented |
| Address probing before OFFER (§3.1) | ✅ | ICMP ping / DAD via `libs/icmp-ping`, per-network `ping_check` |
| Lease time, T1/T2 (§4.4.5) | ✅ | T1 ≈ 0.5·lease, T2 ≈ 0.875·lease; honors requested lease time (opt 51) |
| Server identifier (opt 54) in OFFER/ACK/NAK | ✅ | Set in `message-type`; re-added on NAK |
| Response addressing (§4.1) | ✅ | All giaddr/ciaddr/broadcast/yiaddr cases + ARP injection in `MsgContext::resp_addr` |
| Renew/rebind lease-extension | ✅ | Renew threshold cache in `plugins/leases` |

### Known deviations

#### DHCPREQUEST states are collapsed (§4.3.2)

`requested_ip()` returns `ciaddr` or option 50 without classifying the request,
and `Leases::request` handles all four states identically. Effects:

- **INIT-REBOOT with no binding may produce a spurious NAK.** RFC 2131 §4.3.2:
  if the server has *no record* of the client it **MUST remain silent** (it may
  only NAK if it positively knows the client is on the wrong network). When
  `authoritative`, `dora` NAKs any un-leasable requested IP.
- **Server-id presence rules per state are not enforced.** SELECTING MUST include
  a matching server-id; RENEWING/REBINDING/INIT-REBOOT MUST NOT include one.
  `dora` validates "if present, must match" but does not use presence/absence to
  classify the request or reject malformed ones.
- **RENEWING vs REBINDING are indistinguishable** to the server, so the
  authoritative-NAK logic applies to both identically.

#### Broadcast flag forced on DHCPREQUEST replies (§4.1)

The `message-type` plugin sets the broadcast flag on every non-relayed
DHCPREQUEST reply. The server is meant to *honor* the client's broadcast flag,
not set it. In practice `resp_addr` still unicasts to `ciaddr` when present, so
renewing clients are reached, but the reply carries a flag the client did not
request. Should be narrowed to the SELECTING/INIT-REBOOT case where the client
cannot yet receive unicast.

#### DHCPINFORM (§4.3.5)

INFORM is answered only when the network is `authoritative` **and** a matching
range exists. RFC 2131 says a server responds to INFORM with the client's local
configuration regardless of pools, MUST NOT return a lease time, and MUST set
`yiaddr = 0`. The lease-time / `yiaddr` handling is correct; the
`authoritative` + range gating is a deviation that can suppress a legitimate
INFORM response.

---

## DHCPv4 options — RFC 2132 and extensions

| RFC | Feature | Status | Notes |
| --- | --- | --- | --- |
| 2132 | Standard options, Parameter Request List (opt 55) | ✅ | `populate_opts` honors the PRL |
| 2132 | Option Overload (opt 52) | ❌ | `sname`/`file` written, but an incoming overload option is not parsed |
| 2132 | Maximum DHCP Message Size (opt 57) | ❌ | Ignored; no response-size clamping |
| 1497 | BOOTP vendor extensions | ✅ | |
| 3046 | Relay Agent Information (opt 82) echo | ✅ | Echoed into response in `populate_opts` |
| 3011 | Subnet Selection option | ✅ | `relay_subnet` |
| 3527 | Link Selection sub-option (precedence over 3011) | ✅ | `relay_subnet` |
| 4578 | Client System Architecture / PXE options | ✅ | Via option passthrough |
| 4093 / 6842 | Client Identifier echo | ✅ | `populate_opts` copies opt 61 |
| 5107 | Server Identifier Override sub-option | ✅ | `RespServerId` |

---

## DDNS — RFC 4701 / 4702 / 4703

| RFC | Feature | Status | Notes |
| --- | --- | --- | --- |
| 4701 | DHCID RR rdata | ✅ | `libs/ddns/src/dhcid.rs` |
| 4702 | Client FQDN option (opt 81), server behavior from flags | ✅ | `libs/ddns` |
| 4703 | DHCID conflict resolution in DNS updates | ✅ | `libs/ddns/src/update.rs` |

See [ddns.md](./ddns.md) for configuration and details.

---

## DHCPv6 — RFC 8415 / 3736

| Feature | Status | Notes |
| --- | --- | --- |
| Information-Request → Reply (stateless, RFC 3736) | ✅ | `message-type` v6 handler |
| Server DUID | ✅ | `libs/config/src/v6.rs` |
| ORO (option request) handling, client-id echo | ✅ | `MsgContext::<v6>::populate_opts` |
| Solicit → Advertise → Request → Reply | ✅ | `plugins/leases-v6` |
| IA_NA address assignment | ✅ | `plugins/leases-v6`; pools in v6 `ranges` |
| IA_PD prefix delegation | ✅ | `plugins/leases-v6`; pools in v6 `pd_pools` |
| Renew / Rebind | ✅ | extend-only; NoBinding when unknown |
| Confirm | ✅ | on-link check → Success / NotOnLink |
| Release / Decline (IA) | ✅ | Release frees; Decline → probation |
| Rapid Commit (option 14) | ✅ | v6 `rapid_commit` flag (default off) |
| Status Codes (option 13) | ✅ | NoAddrsAvail / NoPrefixAvail / NoBinding / NotOnLink |
| Duplicate Address Detection | 🟡 | ICMPv6 echo probe (not Neighbor-Solicitation DAD) |
| RelayForw / RelayRepl (§19) | ✅ | `dora-core/server/relay.rs`; nested relays, link-address subnet select, Interface-ID echo |
| IA_TA (temporary addresses) | ❌ | rarely used; out of scope |
| Reconfigure | ❌ | requires Reconfigure Key auth; out of scope |

DHCPv6 is now **stateful**: IA_NA address assignment and IA_PD prefix delegation
with the full Solicit/Request/Renew/Rebind/Confirm/Release/Decline lifecycle
(RFC 8415), implemented in `plugins/leases-v6`, plus relay-agent support
(RelayForw/RelayRepl). IA_TA and Reconfigure remain unimplemented.

> Relay support depends on additions to the `usg-dhcproto` fork (a `RelayMessage`
> constructor and a raw-bytes Relay-Message option), wired via `[patch.crates-io]`
> to a local checkout until those land in a published release.

---

## Prioritized roadmap

1. ~~**DHCPv6 stateful** — IA_NA/IA_PD allocation and the full lifecycle~~ —
   **done** (RFC 8415, `plugins/leases-v6`). See [rfc8415_plan.md](./rfc8415_plan.md).
2. **Split the DHCPREQUEST handler by client state** so INIT-REBOOT stays silent
   on unknown bindings and RENEWING/REBINDING are distinguished (RFC 2131 §4.3.2).
3. **Relax DHCPINFORM gating** so authoritative servers answer INFORM regardless
   of pool coverage (RFC 2131 §4.3.5).
4. **BOOTP 300-byte packet padding** for legacy clients (RFC 951 / 1542).
5. **Parse Option Overload (52)** and honor Maximum Message Size (57).
6. **DHCPv6 follow-ups** — Neighbor-Solicitation DAD, IA_TA, Reconfigure; publish
   the `usg-dhcproto` relay API additions; and end-to-end packet-level v6
   integration tests (incl. the relay path).

---

## References

- [RFC 2131 — DHCP](https://datatracker.ietf.org/doc/html/rfc2131)
- [RFC 2132 — DHCP Options and BOOTP Vendor Extensions](https://datatracker.ietf.org/doc/html/rfc2132)
- [RFC 8415 — DHCPv6](https://datatracker.ietf.org/doc/html/rfc8415)
- [RFC 3736 — Stateless DHCPv6](https://www.rfc-editor.org/rfc/rfc3736.html)
- See the [README](../README.md#rfcs-implemented-in-dora) for the full list of referenced RFCs.
