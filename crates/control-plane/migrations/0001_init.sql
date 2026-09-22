-- aho cloud control plane: identities, credentials, orgs, projects, secrets, uploads.
-- Tokens and device codes are stored as blake3 hashes only; secret values only as ciphertext.

CREATE TABLE users (
    id    TEXT PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    name  TEXT NOT NULL
);

CREATE TABLE device_codes (
    code_hash   TEXT PRIMARY KEY,
    user_code   TEXT NOT NULL UNIQUE,
    device_name TEXT NOT NULL,
    public_key  TEXT NOT NULL,
    expires_at  BIGINT NOT NULL,
    approved_by TEXT REFERENCES users (id)
);

CREATE TABLE tokens (
    hash       TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users (id),
    expires_at BIGINT NOT NULL
);

CREATE TABLE device_keys (
    key_id     TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users (id),
    name       TEXT NOT NULL,
    public_key TEXT NOT NULL,
    created_at BIGINT NOT NULL
);
CREATE INDEX device_keys_user ON device_keys (user_id);

CREATE TABLE passkeys (
    credential_id TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL REFERENCES users (id),
    public_key    BYTEA NOT NULL,
    sign_count    BIGINT NOT NULL,
    created_at    BIGINT NOT NULL
);
CREATE INDEX passkeys_user ON passkeys (user_id);

CREATE TABLE ceremonies (
    id         TEXT PRIMARY KEY,
    kind       TEXT NOT NULL,
    challenge  BYTEA NOT NULL,
    user_id    TEXT NOT NULL,
    email      TEXT NOT NULL,
    name       TEXT NOT NULL,
    new_user   BOOLEAN NOT NULL,
    expires_at BIGINT NOT NULL
);

CREATE TABLE orgs (
    slug            TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    policy          TEXT NOT NULL,
    policy_approver TEXT
);

CREATE TABLE members (
    org     TEXT NOT NULL REFERENCES orgs (slug),
    user_id TEXT NOT NULL REFERENCES users (id),
    owner   BOOLEAN NOT NULL,
    PRIMARY KEY (org, user_id)
);
CREATE INDEX members_user ON members (user_id);

CREATE TABLE projects (
    slug       TEXT PRIMARY KEY,
    org        TEXT NOT NULL REFERENCES orgs (slug),
    created_at BIGINT NOT NULL,
    deleted_at BIGINT
);

CREATE TABLE secrets (
    project     TEXT NOT NULL REFERENCES projects (slug),
    name        TEXT NOT NULL,
    stage       TEXT NOT NULL,
    ciphertext  BYTEA NOT NULL,
    fingerprint TEXT NOT NULL,
    edge_phc    TEXT,
    updated_at  BIGINT NOT NULL,
    PRIMARY KEY (project, name, stage)
);

CREATE TABLE blobs (
    project    TEXT NOT NULL REFERENCES projects (slug),
    hash       TEXT NOT NULL,
    kind       TEXT NOT NULL,
    size       BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (project, hash)
);
CREATE INDEX blobs_hash ON blobs (hash);

CREATE TABLE sessions (
    project    TEXT NOT NULL REFERENCES projects (slug),
    session_id TEXT NOT NULL,
    head_hash  TEXT NOT NULL,
    blob_hash  TEXT NOT NULL,
    entries    BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (project, session_id, head_hash)
);

CREATE TABLE release_meta (
    project    TEXT NOT NULL REFERENCES projects (slug),
    release    TEXT NOT NULL,
    edge_toml  TEXT,
    build      TEXT NOT NULL,
    build_log  TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (project, release)
);

CREATE TABLE kv (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
