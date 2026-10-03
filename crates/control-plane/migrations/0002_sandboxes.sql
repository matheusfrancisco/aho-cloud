-- Sandboxes: VMs with a persistent disk, owned by an org. Their slugs share one namespace with
-- project slugs (enforced by the store under a per-slug advisory lock). Environment values only
-- as ciphertext.

CREATE TABLE sandboxes (
    slug       TEXT PRIMARY KEY,
    org        TEXT NOT NULL REFERENCES orgs (slug),
    created_by TEXT NOT NULL REFERENCES users (id),
    created_at BIGINT NOT NULL,
    image      TEXT NOT NULL,
    settings   TEXT NOT NULL,
    state      TEXT NOT NULL,
    error      TEXT,
    stops_at   BIGINT,
    cpus       BIGINT NOT NULL,
    mem_mib    BIGINT NOT NULL,
    disk_mib   BIGINT NOT NULL
);
CREATE INDEX sandboxes_org ON sandboxes (org);

CREATE TABLE sandbox_env (
    sandbox     TEXT NOT NULL REFERENCES sandboxes (slug) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    ciphertext  BYTEA NOT NULL,
    fingerprint TEXT NOT NULL,
    updated_at  BIGINT NOT NULL,
    PRIMARY KEY (sandbox, name)
);
