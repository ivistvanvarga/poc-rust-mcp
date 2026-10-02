-- Persisted history of calculator tool invocations.
--
-- `inputs` keeps the raw JSON arguments the client sent, so a tool signature can evolve
-- without rewriting old rows. `result` and `error` are mutually exclusive.
CREATE TABLE IF NOT EXISTS calc_history (
    id BIGSERIAL PRIMARY KEY,
    operation TEXT NOT NULL,
    inputs JSONB NOT NULL,
    result TEXT,
    error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT calc_history_result_xor_error CHECK (
        (result IS NULL) <> (error IS NULL)
    )
);

CREATE INDEX IF NOT EXISTS calc_history_created_at_idx ON calc_history (created_at DESC);