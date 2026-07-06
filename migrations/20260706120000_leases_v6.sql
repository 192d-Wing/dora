-- DHCPv6 stateful leases (RFC 8415).
--
-- Kept separate from the v4 `leases` table because a 128-bit address does not
-- fit the v4 `ip INTEGER` column, and to avoid disturbing the v4 hot path.
--
-- `addr` is a 16-byte big-endian IPv6 address (IA_NA) or the base of a delegated
-- prefix (IA_PD). `prefix_len` is 128 for IA_NA addresses and the delegated
-- length for IA_PD prefixes, so (addr, prefix_len) uniquely identifies a
-- binding. `client_id` carries the DHCPv6 identity (DUID, plus IAID for the
-- binding). `leased`/`probation` mirror the v4 tri-state (see IpState).
CREATE TABLE IF NOT EXISTS leases_v6(
    addr        BLOB    NOT NULL,
    prefix_len  INTEGER NOT NULL DEFAULT 128,
    client_id   BLOB,
    leased      BOOLEAN NOT NULL DEFAULT 0,
    probation   BOOLEAN NOT NULL DEFAULT 0,
    expires_at  INTEGER NOT NULL,
    network     BLOB    NOT NULL,
    PRIMARY KEY(addr, prefix_len)
);
CREATE INDEX IF NOT EXISTS idx_v6_client_id on leases_v6 (client_id, expires_at);
CREATE INDEX IF NOT EXISTS idx_v6_addr_expires on leases_v6 (addr, expires_at);
