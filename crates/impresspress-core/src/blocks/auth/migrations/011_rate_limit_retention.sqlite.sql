-- Bound the durable D1 rate-counter table by making age pruning indexed.
CREATE INDEX IF NOT EXISTS idx_auth_rate_limits_updated_at
    ON wafer_run__auth__rate_limits (updated_at);
