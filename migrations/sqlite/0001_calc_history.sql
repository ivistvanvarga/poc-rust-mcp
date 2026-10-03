-- Persisted history of calculator tool invocations.
--
-- The SQLite twin of ../postgres/0001_calc_history.sql: same columns, same invariants, expressed
-- in the dialect sqlx needs for this backend.
--
-- `inputs` keeps the raw JSON arguments the client sent, so a tool signature can evolve
-- without rewriting old rows. `result` and `error` are mutually exclusive.
--
-- SQLite stores JSON and timestamps as TEXT, so `created_at` is a string, and unlike PostgreSQL's
-- `now()` and MySQL's `CURRENT_TIMESTAMP` there is no `DEFAULT` here on purpose. SQLite's own
-- `CURRENT_TIMESTAMP` writes `2026-10-03 12:00:00`, whose space separator sorts before the `T` of
-- every RFC 3339 timestamp, so mixing the two would silently corrupt `ORDER BY created_at DESC`.
-- Requiring an explicit value keeps one format in every row. The server always writes RFC 3339 UTC
-- (see `Store::insert`), which sorts correctly lexicographically.
CREATE TABLE IF NOT EXISTS calc_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    operation TEXT NOT NULL,
    inputs TEXT NOT NULL,
    result TEXT,
    error TEXT,
    created_at TEXT NOT NULL,
    CONSTRAINT calc_history_result_xor_error CHECK (
        (result IS NULL) <> (error IS NULL)
    )
);

CREATE INDEX IF NOT EXISTS calc_history_created_at_idx ON calc_history (created_at DESC);
