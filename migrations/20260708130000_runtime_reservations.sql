-- Runtime (API-managed) host reservations.
--
-- These are created/updated/deleted through the management API's
-- create/update/delete-reservation actions and take precedence over config
-- reservations and the dynamic pool in the DHCP datapath. They are persisted
-- here so they survive restarts; on startup the server loads this table into an
-- in-memory store the datapath reads on the hot path.
--
-- `family` is 'v4' or 'v6'. `ip` is the reserved address (dotted v4 / canonical
-- v6) and, together with `family`, uniquely identifies a reservation — the
-- delete action keys on (family, ip). `prefix` is an optional v6 IA_PD
-- delegation ("base/len"). `network` is the optional owning network (CIDR).
-- `match_json` is the serialized match predicate (opaque to storage): for v4 the
-- `config::wire::v4::Condition` JSON (`{"chaddr": ...}` / `{"options": ...}`),
-- for v6 a `{"duid": <hex>}` / `{"mac": <mac>}` object. `created_at` is unix
-- epoch seconds, matching the other tables.
CREATE TABLE IF NOT EXISTS runtime_reservations(
    family     TEXT    NOT NULL,
    ip         TEXT    NOT NULL,
    prefix     TEXT,
    network    TEXT,
    match_json TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (family, ip)
);
