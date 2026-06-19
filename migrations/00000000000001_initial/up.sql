CREATE TABLE folders (
    id          TEXT    PRIMARY KEY NOT NULL,
    name        TEXT    NOT NULL,
    path        TEXT    NOT NULL UNIQUE,
    created_at  TEXT    NOT NULL
);

CREATE TABLE sessions (
    id              TEXT    PRIMARY KEY NOT NULL,
    name            TEXT    NOT NULL,
    folder_id       TEXT    NOT NULL REFERENCES folders(id),
    model           TEXT,
    effort          TEXT,
    is_worker       BOOLEAN NOT NULL DEFAULT 0,
    project_id      TEXT    REFERENCES projects(id),
    card_id         TEXT    REFERENCES cards(id),
    conversation_id TEXT,
    created_at      TEXT    NOT NULL,
    last_activity   TEXT    NOT NULL
);

CREATE TABLE projects (
    id                      TEXT    PRIMARY KEY NOT NULL,
    name                    TEXT    NOT NULL,
    context                 TEXT    NOT NULL DEFAULT '',
    folder_id               TEXT    NOT NULL REFERENCES folders(id),
    worker_count            INTEGER NOT NULL DEFAULT 1,
    status                  TEXT    NOT NULL DEFAULT 'active',
    default_workflow        TEXT,
    model                   TEXT,
    effort                  TEXT,
    parallel_instructions   BOOLEAN NOT NULL DEFAULT 0,
    created_at              TEXT    NOT NULL,
    last_accessed_at        TEXT    NOT NULL
);

CREATE INDEX idx_sessions_folder ON sessions (folder_id);
CREATE INDEX idx_projects_folder ON projects (folder_id);

CREATE TABLE cards (
    id                      TEXT    PRIMARY KEY NOT NULL,
    project_id              TEXT    NOT NULL REFERENCES projects(id),
    title                   TEXT    NOT NULL,
    description             TEXT    NOT NULL DEFAULT '',
    step                    TEXT    NOT NULL DEFAULT 'backlog',
    priority                INTEGER NOT NULL DEFAULT 3,
    workflow                TEXT,
    model                   TEXT,
    effort                  TEXT,
    worker_session_id       TEXT    REFERENCES sessions(id),
    last_worker_session_id  TEXT    REFERENCES sessions(id),
    handoff_context         TEXT,
    blocked                 BOOLEAN NOT NULL DEFAULT 0,
    block_reason            TEXT,
    created_at              TEXT    NOT NULL,
    updated_at              TEXT    NOT NULL
);

CREATE TABLE events (
    id          TEXT    PRIMARY KEY NOT NULL,
    session_id  TEXT    NOT NULL REFERENCES sessions(id),
    seq         INTEGER NOT NULL,
    ts          INTEGER NOT NULL,
    kind        TEXT    NOT NULL,
    data        TEXT    NOT NULL DEFAULT '{}',
    UNIQUE (session_id, seq)
);

CREATE INDEX idx_events_session ON events (session_id, seq);

CREATE TABLE users (
    id              TEXT    PRIMARY KEY NOT NULL,
    username        TEXT    NOT NULL UNIQUE,
    email           TEXT    UNIQUE,
    password_hash   TEXT    NOT NULL,
    role            TEXT    NOT NULL DEFAULT 'user',
    created_at      TEXT    NOT NULL,
    updated_at      TEXT    NOT NULL
);

CREATE TABLE auth_sessions (
    id              TEXT    PRIMARY KEY NOT NULL,
    user_id         TEXT    NOT NULL REFERENCES users(id),
    token_hash      TEXT    NOT NULL,
    created_at      INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL,
    last_used_at    INTEGER,
    user_agent      TEXT,
    ip_address      TEXT
);

CREATE INDEX idx_auth_sessions_user ON auth_sessions (user_id);

CREATE TABLE push_subscriptions (
    endpoint    TEXT    PRIMARY KEY NOT NULL,
    p256dh      TEXT    NOT NULL,
    auth_key    TEXT    NOT NULL,
    created_at  TEXT    NOT NULL
);

CREATE TABLE queued_messages (
    session_id  TEXT    PRIMARY KEY NOT NULL REFERENCES sessions(id),
    text        TEXT    NOT NULL,
    queued_at   TEXT    NOT NULL
);

CREATE TABLE announcements (
    id          TEXT    PRIMARY KEY NOT NULL,
    kind        TEXT    NOT NULL,
    title       TEXT    NOT NULL,
    message     TEXT    NOT NULL,
    detail      TEXT,
    created_at  TEXT    NOT NULL
);
