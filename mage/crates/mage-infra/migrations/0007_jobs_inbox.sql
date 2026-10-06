-- Ops: job queue (the transactional outbox) and webhook inbox (storage.md "Ops", "Job queue
-- semantics", "Webhook inbox semantics"). Wakeups on NOTIFY `mage_jobs` are sent by
-- `mage_infra::pg::jobs::enqueue_in`, not by a trigger.

CREATE TABLE jobs (
  id            uuid PRIMARY KEY,
  org_id        uuid NULL REFERENCES orgs (id),           -- NULL only for platform jobs
  kind          text NOT NULL,                            -- JobKind::as_str()
  lane          text NOT NULL CHECK (lane IN ('server','indexer')),
  payload       jsonb NOT NULL,                           -- JobPayload
  dedupe_key    text NULL,
  priority      smallint NOT NULL DEFAULT 0,
  state         text NOT NULL CHECK (state IN ('queued','running','succeeded','dead','cancelled')),
  run_after     timestamptz NOT NULL,
  attempts      smallint NOT NULL DEFAULT 0,              -- leases counted against max_attempts
  max_attempts  smallint NOT NULL,
  defers        smallint NOT NULL DEFAULT 0,
  lease_seq     int NOT NULL DEFAULT 0,                   -- fencing token: +1 per lease, never decremented
  leased_by     text NULL,
  leased_until  timestamptz NULL,
  cancel_requested boolean NOT NULL DEFAULT false,
  last_error    text NULL,                                -- truncated to 4 KiB
  created_at timestamptz NOT NULL, updated_at timestamptz NOT NULL, finished_at timestamptz NULL,
  CHECK ((state = 'running') = (leased_until IS NOT NULL)),
  CHECK ((state IN ('succeeded','dead','cancelled')) = (finished_at IS NOT NULL))
) WITH (fillfactor = 80, autovacuum_vacuum_scale_factor = 0.01);   -- leases and heartbeats
-- at most one active job per (org, kind, dedupe key); NULL org (platform jobs) dedupes too
CREATE UNIQUE INDEX jobs_dedupe_active ON jobs (org_id, kind, dedupe_key) NULLS NOT DISTINCT
  WHERE dedupe_key IS NOT NULL AND state IN ('queued','running');
CREATE INDEX jobs_ready    ON jobs (lane, priority DESC, run_after, id) WHERE state = 'queued';
CREATE INDEX jobs_expiry   ON jobs (leased_until) WHERE state = 'running';
CREATE INDEX jobs_dead     ON jobs (kind, finished_at) WHERE state = 'dead';
CREATE INDEX jobs_finished ON jobs (finished_at) WHERE state IN ('succeeded','cancelled');
CREATE INDEX jobs_org      ON jobs (org_id) WHERE state IN ('queued','running');
SELECT mage_platform_table('jobs');
-- outbox: tenant transactions insert jobs and look up the active duplicate (storage.md "Tenancy")
GRANT INSERT ON jobs TO mage_tenant;
GRANT SELECT (id, org_id, kind, dedupe_key, state) ON jobs TO mage_tenant;

CREATE TABLE webhook_inbox (
  source      text NOT NULL CHECK (source IN ('github','github_fix','linear','slack')),
  delivery_id text NOT NULL,
  org_id      uuid NULL REFERENCES orgs (id),             -- set by resolve_org
  event       text NOT NULL,
  body        bytea NULL,                                 -- raw verified body; NULL after purge
  body_sha256 bytea NOT NULL CHECK (octet_length(body_sha256) = 32),
  body_bytes  int NOT NULL,
  received_at timestamptz NOT NULL,
  job_id      uuid NULL,
  outcome     text NULL CHECK (outcome IN ('processed','ignored','rejected')),
  outcome_note text NULL,
  processed_at timestamptz NULL,
  PRIMARY KEY (source, delivery_id),
  UNIQUE (source, body_sha256),                           -- replay under a forged delivery id is a duplicate
  CHECK ((outcome IS NULL) = (processed_at IS NULL))
) WITH (fillfactor = 80, autovacuum_vacuum_scale_factor = 0.01);
CREATE INDEX webhook_inbox_rows   ON webhook_inbox (received_at);
CREATE INDEX webhook_inbox_bodies ON webhook_inbox (processed_at) WHERE body IS NOT NULL AND processed_at IS NOT NULL;
CREATE INDEX webhook_inbox_org    ON webhook_inbox (org_id) WHERE org_id IS NOT NULL;
SELECT mage_platform_table('webhook_inbox');
