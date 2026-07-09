-- Add migration script here
-- would prefer to use an enum where the entry is either leased or
-- on probation. expires_at for lease = true refers to when lease expires,
-- if probabtion = true it is when the probation expires
--
-- `ip` and `network` are IPv4 addresses held as their 32-bit numeric value in a
-- BIGINT (Postgres INTEGER is signed 32-bit and would overflow addresses above
-- 2147483647, e.g. 255.255.255.255). `client_id` is the raw DHCP client id.
-- Timestamps are unix epoch seconds.
CREATE TABLE IF NOT EXISTS leases(
    ip BIGINT NOT NULL,
    client_id BYTEA,
    leased BOOLEAN NOT NULL DEFAULT FALSE,
    expires_at BIGINT NOT NULL,
    network BIGINT NOT NULL,
    probation BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY(ip)
);
CREATE INDEX IF NOT EXISTS idx_ip_expires on leases (ip, expires_at);
