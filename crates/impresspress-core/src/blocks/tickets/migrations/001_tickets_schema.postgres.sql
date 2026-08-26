-- Tickets workflow schema (PostgreSQL parity).

CREATE TABLE IF NOT EXISTS impresspress__tickets__types (
    id                  TEXT PRIMARY KEY,
    key                 TEXT NOT NULL UNIQUE,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    guidance            TEXT NOT NULL DEFAULT '',
    default_priority    TEXT NOT NULL DEFAULT 'normal'
                            CHECK (default_priority IN ('low', 'normal', 'high', 'urgent')),
    escalation_kind     TEXT NOT NULL DEFAULT 'none'
                            CHECK (escalation_kind IN ('none', 'legal', 'privacy', 'safety')),
    public_visible      INTEGER NOT NULL DEFAULT 0 CHECK (public_visible IN (0, 1)),
    requires_contact    INTEGER NOT NULL DEFAULT 0 CHECK (requires_contact IN (0, 1)),
    requests_evidence   INTEGER NOT NULL DEFAULT 0 CHECK (requests_evidence IN (0, 1)),
    active              INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    sort_order          INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS impresspress__tickets__tickets (
    id                      TEXT PRIMARY KEY,
    reference               TEXT NOT NULL UNIQUE,
    type_id                 TEXT NOT NULL,
    type_key_snapshot       TEXT NOT NULL,
    type_title_snapshot     TEXT NOT NULL,
    source                  TEXT NOT NULL
                                CHECK (source IN ('public_form', 'admin', 'api', 'ai')),
    status                  TEXT NOT NULL DEFAULT 'new'
                                CHECK (status IN ('new', 'triaged', 'investigating', 'resolved', 'rejected', 'spam', 'duplicate')),
    priority                TEXT NOT NULL DEFAULT 'normal'
                                CHECK (priority IN ('low', 'normal', 'high', 'urgent')),
    subject                 TEXT NOT NULL,
    description             TEXT NOT NULL,
    source_path             TEXT NOT NULL DEFAULT '',
    subject_type            TEXT NOT NULL DEFAULT '',
    subject_id              TEXT NOT NULL DEFAULT '',
    evidence_url            TEXT NOT NULL DEFAULT '',
    reporter_email          TEXT NOT NULL DEFAULT '',
    reporter_wants_reply    INTEGER NOT NULL DEFAULT 0 CHECK (reporter_wants_reply IN (0, 1)),
    assignee_id             TEXT NOT NULL DEFAULT '',
    duplicate_of            TEXT,
    legal_hold              INTEGER NOT NULL DEFAULT 0 CHECK (legal_hold IN (0, 1)),
    dedupe_hash             TEXT,
    resolved_at             TEXT,
    expires_at              TEXT,
    created_at              TEXT NOT NULL,
    updated_at              TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS impresspress__tickets__events (
    id              TEXT PRIMARY KEY,
    ticket_id       TEXT NOT NULL,
    event_type      TEXT NOT NULL,
    actor_type      TEXT NOT NULL
                        CHECK (actor_type IN ('public', 'admin', 'api', 'ai', 'system')),
    actor_id        TEXT NOT NULL DEFAULT '',
    body            TEXT NOT NULL DEFAULT '',
    metadata_json   TEXT NOT NULL DEFAULT '{}',
    expires_at      TEXT,
    created_at      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS impresspress__tickets__analyses (
    id                          TEXT PRIMARY KEY,
    ticket_id                   TEXT NOT NULL,
    source                      TEXT NOT NULL,
    model                       TEXT,
    prompt_version              TEXT NOT NULL DEFAULT '',
    summary                     TEXT NOT NULL,
    suggested_type_id           TEXT,
    suggested_priority          TEXT
                                    CHECK (suggested_priority IS NULL OR suggested_priority IN ('low', 'normal', 'high', 'urgent')),
    confidence                  DOUBLE PRECISION NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
    suggested_actions_json      TEXT NOT NULL DEFAULT '[]',
    expires_at                  TEXT,
    created_at                  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS impresspress__tickets__maintenance (
    id                  TEXT PRIMARY KEY,
    last_pruned_day     TEXT NOT NULL DEFAULT '',
    last_pruned_at      TEXT,
    last_prune_error    TEXT NOT NULL DEFAULT '',
    audit_degraded      INTEGER NOT NULL DEFAULT 0 CHECK (audit_degraded IN (0, 1))
);

INSERT INTO impresspress__tickets__maintenance (id)
VALUES ('singleton')
ON CONFLICT (id) DO NOTHING;

CREATE INDEX IF NOT EXISTS idx_tickets_types_public
    ON impresspress__tickets__types (active, public_visible, sort_order);
CREATE INDEX IF NOT EXISTS idx_tickets_status_priority_created
    ON impresspress__tickets__tickets (status, priority, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_tickets_type_status_created
    ON impresspress__tickets__tickets (type_id, status, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_tickets_source_status_created
    ON impresspress__tickets__tickets (source, status, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_tickets_assignee_status_updated
    ON impresspress__tickets__tickets (assignee_id, status, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_tickets_expires
    ON impresspress__tickets__tickets (expires_at);
CREATE UNIQUE INDEX IF NOT EXISTS idx_tickets_dedupe
    ON impresspress__tickets__tickets (dedupe_hash);
CREATE INDEX IF NOT EXISTS idx_ticket_events_ticket_created
    ON impresspress__tickets__events (ticket_id, created_at);
CREATE INDEX IF NOT EXISTS idx_ticket_events_expires
    ON impresspress__tickets__events (expires_at);
CREATE INDEX IF NOT EXISTS idx_ticket_analyses_ticket_created
    ON impresspress__tickets__analyses (ticket_id, created_at);
CREATE INDEX IF NOT EXISTS idx_ticket_analyses_expires
    ON impresspress__tickets__analyses (expires_at);
