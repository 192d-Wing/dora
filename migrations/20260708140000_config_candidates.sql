-- Staged configuration candidates for the config-lifecycle API.
--
-- `PUT /v1/config` / `POST /v1/config/candidates` insert a `staged` candidate,
-- which is validated (parsed as a DhcpConfig) to `valid` / `invalid`.
-- `activate-config` writes a valid candidate's document to the config file
-- atomically, marks it `activated` (superseding the previous active candidate),
-- and triggers a graceful restart so the datapath adopts it. `rollback-config`
-- re-activates a previously-activated candidate by id. The single row with
-- status `activated` is the current active config; rows with a non-null
-- `activated_at` form the activation history for rollback.
--
-- `document` is the candidate config as text (YAML). `validation` is a
-- JSON array of {level, path?, message}. Timestamps are unix epoch seconds.
CREATE TABLE IF NOT EXISTS config_candidates(
    candidate_id TEXT    NOT NULL PRIMARY KEY,
    status       TEXT    NOT NULL,
    document     TEXT    NOT NULL,
    message      TEXT,
    validation   TEXT,
    created_at   BIGINT  NOT NULL,
    activated_at BIGINT
);
CREATE INDEX IF NOT EXISTS idx_config_candidates_created ON config_candidates (created_at);
