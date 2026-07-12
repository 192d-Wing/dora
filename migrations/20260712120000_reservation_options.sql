-- Per-reservation DHCP attributes for API-managed (runtime) reservations.
--
-- v4 reservations may now carry the same options / class / lease-time overrides
-- that config reservations support, so an API-created reservation can hand out
-- specific options (not just pin an address). All three are NULL for v6
-- reservations, which only pin an address/prefix.
--
-- `options_json` is the serialized v4 options (`{"values": {...}}`, the same
-- shape as config), opaque to storage. `class` restricts the reservation to a
-- matched client class. `lease_time` is a lease override in seconds.
ALTER TABLE runtime_reservations ADD COLUMN IF NOT EXISTS options_json TEXT;
ALTER TABLE runtime_reservations ADD COLUMN IF NOT EXISTS class        TEXT;
ALTER TABLE runtime_reservations ADD COLUMN IF NOT EXISTS lease_time   BIGINT;
