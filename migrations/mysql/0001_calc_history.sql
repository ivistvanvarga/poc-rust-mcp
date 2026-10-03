-- Persisted history of calculator tool invocations.
--
-- The MySQL/MariaDB twin of ../postgres/0001_calc_history.sql: same columns, same invariants,
-- expressed in the dialect sqlx needs for this backend. Requires MySQL 8.0.16+ or MariaDB 10.2.1+
-- so that the CHECK constraint is actually enforced and JSON columns exist.
--
-- `inputs` keeps the raw JSON arguments the client sent, so a tool signature can evolve
-- without rewriting old rows. `result` and `error` are mutually exclusive.
--
-- `created_at` is DATETIME rather than TIMESTAMP on purpose: TIMESTAMP is silently converted
-- between the session time zone and UTC on the way in and out, which would shift every row by the
-- server's offset. The server always writes UTC (see `Store::insert`).
CREATE TABLE IF NOT EXISTS calc_history (
    id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    operation VARCHAR(32) NOT NULL,
    inputs JSON NOT NULL,
    result TEXT,
    error TEXT,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    CONSTRAINT calc_history_result_xor_error CHECK (
        (result IS NULL) <> (error IS NULL)
    )
);

-- MySQL has no `CREATE INDEX IF NOT EXISTS`. That is fine: sqlx records applied migrations in
-- `_sqlx_migrations`, and DDL is non-transactional here, so re-running this file can only happen
-- after a partial failure -- in which case the CREATE TABLE above is a no-op and this succeeds.
CREATE INDEX calc_history_created_at_idx ON calc_history (created_at DESC);
