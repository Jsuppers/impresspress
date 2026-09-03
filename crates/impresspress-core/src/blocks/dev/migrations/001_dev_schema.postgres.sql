-- Browser development sandbox control plane (PostgreSQL parity).
--
-- Three tables, all owned by `impresspress/dev`:
--   generations   — the append-only publication ledger (§7.2)
--   builds        — one row per staged compiler artifact (§11.1)
--   runtime_state — the single-row activation journal (§11.1)

CREATE TABLE IF NOT EXISTS impresspress__dev__generations (
    id                  TEXT PRIMARY KEY,
    parent_id           TEXT,
    status              TEXT NOT NULL
                            CHECK (status IN ('staged', 'validating', 'activating', 'active', 'failed', 'superseded')),
    cause               TEXT NOT NULL
                            CHECK (cause IN ('site_write', 'site_delete', 'block_compile', 'block_remove', 'rollback', 'seed')),
    site_manifest_json  TEXT NOT NULL,
    block_manifest_json TEXT NOT NULL,
    manifest_sha256     TEXT NOT NULL,
    created_at          TEXT NOT NULL,
    activated_at        TEXT,
    failure_message     TEXT
);

CREATE INDEX IF NOT EXISTS idx_impresspress__dev__generations_created
    ON impresspress__dev__generations(created_at);

CREATE TABLE IF NOT EXISTS impresspress__dev__builds (
    id                      TEXT PRIMARY KEY,
    block_name              TEXT NOT NULL,
    source_manifest_sha256  TEXT NOT NULL,
    artifact_sha256         TEXT NOT NULL,
    block_info_json         TEXT NOT NULL,
    diagnostics_json        TEXT NOT NULL,
    compiler_version        TEXT NOT NULL,
    status                  TEXT NOT NULL
                                CHECK (status IN ('staged', 'valid', 'invalid')),
    created_at              TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_impresspress__dev__builds_created
    ON impresspress__dev__builds(created_at);

-- Single-row activation journal. `singleton_id` is the primary key and is
-- pinned to 1 by the CHECK, so the table can hold exactly one row; the repo
-- module addresses it by that column rather than by a generated `id`.
CREATE TABLE IF NOT EXISTS impresspress__dev__runtime_state (
    singleton_id            INTEGER PRIMARY KEY CHECK (singleton_id = 1),
    active_generation_id    TEXT,
    desired_generation_id   TEXT,
    activation_phase        TEXT NOT NULL
                                CHECK (activation_phase IN ('idle', 'validating', 'building_runtime', 'publishing', 'active', 'failed')),
    generation              BIGINT NOT NULL,
    updated_at              TEXT NOT NULL
);

INSERT INTO impresspress__dev__runtime_state
    (singleton_id, active_generation_id, desired_generation_id, activation_phase, generation, updated_at)
VALUES (1, NULL, NULL, 'idle', 0, '1970-01-01T00:00:00Z')
ON CONFLICT (singleton_id) DO NOTHING;
