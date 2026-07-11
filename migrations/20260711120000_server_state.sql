-- Cluster-wide server mode, shared across the split services.
--
-- Since v4/v6/api run as separate processes, the management API's maintenance /
-- drain mode change has to be durable somewhere both sides can see it. This
-- single-row table holds the current mode; the API writes it, and the DHCP
-- servers poll it into their in-memory SharedMode.
CREATE TABLE IF NOT EXISTS server_state (
    -- singleton: exactly one row, id is always TRUE
    id boolean PRIMARY KEY DEFAULT TRUE,
    mode text NOT NULL DEFAULT 'normal',
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT server_state_singleton CHECK (id)
);

INSERT INTO server_state (id, mode) VALUES (TRUE, 'normal')
ON CONFLICT (id) DO NOTHING;
