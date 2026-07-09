-- Async operation / audit records for the management API.
--
-- Every mutating management action (maintenance-mode, drain, shutdown, and the
-- config/reservation/lease actions added in later phases) records one row here.
-- Synchronous actions insert a row that is already terminal (succeeded/failed);
-- asynchronous actions insert an `accepted` row up front and update it as the
-- work runs, so `GET /v1/operations/{id}` can report lifecycle status. Rows are
-- retained as an audit trail until normal log/audit retention prunes them.
--
-- `status` is one of: accepted, running, succeeded, failed, canceled.
-- `actor` summarizes the caller's auth context (e.g. "bearer"), not a secret.
-- `input_summary`, `result`, and `error_*` are redacted, already-serialized JSON
-- text (or NULL) — the storage layer treats them as opaque. Timestamps are unix
-- epoch seconds, matching the `expires_at` convention in the leases tables.
CREATE TABLE IF NOT EXISTS operations(
    operation_id  TEXT    NOT NULL PRIMARY KEY,
    action        TEXT    NOT NULL,
    status        TEXT    NOT NULL,
    actor         TEXT,
    input_summary TEXT,
    result        TEXT,
    error_code    TEXT,
    error_message TEXT,
    created_at    BIGINT  NOT NULL,
    started_at    BIGINT,
    completed_at  BIGINT
);
-- listing/pruning is newest-first by creation time
CREATE INDEX IF NOT EXISTS idx_operations_created_at ON operations (created_at);
