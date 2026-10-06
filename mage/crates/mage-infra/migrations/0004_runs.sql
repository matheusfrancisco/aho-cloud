-- Runs area (storage.md "Runs", columns per runs.md). The FKs runs -> tickets / causes are added in
-- 0005 once those tables exist.

CREATE TABLE runners (                                    -- platform table
  id uuid PRIMARY KEY,
  -- identity (TokenStore)
  name text NOT NULL UNIQUE,
  scope jsonb NOT NULL,                                   -- RunnerScope: Shared | Orgs([..]), copied from the bootstrap token
  status text NOT NULL CHECK (status IN ('active','draining','revoked')),
  public_key bytea NOT NULL CHECK (octet_length(public_key) = 32),   -- Ed25519
  generation int NOT NULL DEFAULT 1,
  registered_at timestamptz NOT NULL,
  -- session (RunStore::record_runner_session)
  instance_id uuid NULL, capabilities jsonb NOT NULL DEFAULT '{}', version text NULL,
  last_seen_at timestamptz NULL
);
SELECT mage_platform_table('runners');
GRANT SELECT ON runners TO mage_tenant;                  -- run views show the holding runner

CREATE TABLE runs (
  org_id uuid NOT NULL REFERENCES orgs (id), id uuid NOT NULL,
  kind   text NOT NULL CHECK (kind IN ('learn','triage','diagnose','verify','fix')),
  ticket_id uuid NULL, cause_id uuid NULL, parent_run_id uuid NULL,      -- subject + denormalized target
  repo_id uuid NULL, sha bytea NULL CHECK (octet_length(sha) = 20),
  harness text NULL CHECK (harness IN ('omp','claude_code')),
  status  text NOT NULL CHECK (status IN ('queued','leased','preparing','running','awaiting_gate',
                                          'succeeded','failed','timed_out','cancelled','errored')),
  attempt  smallint NOT NULL,
  version  bigint NOT NULL,                               -- optimistic concurrency (RunStore::commit)
  spec     jsonb NOT NULL,                                -- RunSpec snapshot
  slot_class text NOT NULL CHECK (slot_class IN ('work','light','verify')),
  priority smallint NOT NULL,
  source_ready boolean NOT NULL,
  budget jsonb NOT NULL, usage_reported jsonb NOT NULL, usage_metered jsonb NOT NULL,
  wall_accum_ms bigint NOT NULL DEFAULT 0, wall_segment_start timestamptz NULL,
  runner_id uuid NULL REFERENCES runners (id), lease_until timestamptz NULL,
  lease_request_id uuid NULL, runner_instance uuid NULL,
  session_lost boolean NOT NULL DEFAULT false,
  affinity_runner uuid NULL, affinity_until timestamptz NULL,
  next_seq bigint NOT NULL DEFAULT 1, next_cmd_seq bigint NOT NULL DEFAULT 1,
  ingest_attempt int NULL, ingest_local_seq bigint NULL,  -- ingest cursor (runner event dedupe)
  result jsonb NULL, error jsonb NULL,
  idempotency_key text NULL,
  created_at timestamptz NOT NULL, started_at timestamptz NULL, finished_at timestamptz NULL,
  PRIMARY KEY (org_id, id),
  FOREIGN KEY (org_id, parent_run_id) REFERENCES runs (org_id, id),
  FOREIGN KEY (org_id, repo_id)       REFERENCES repos (org_id, id),
  CHECK (CASE kind
    WHEN 'learn'    THEN repo_id IS NOT NULL AND sha IS NOT NULL
    WHEN 'triage'   THEN ticket_id IS NOT NULL
    WHEN 'diagnose' THEN cause_id IS NOT NULL AND repo_id IS NOT NULL AND sha IS NOT NULL
    WHEN 'fix'      THEN cause_id IS NOT NULL AND repo_id IS NOT NULL AND sha IS NOT NULL
    WHEN 'verify'   THEN parent_run_id IS NOT NULL AND repo_id IS NOT NULL AND sha IS NOT NULL
  END)
) WITH (fillfactor = 80, autovacuum_vacuum_scale_factor = 0.01);   -- leases, heartbeats, usage
CREATE UNIQUE INDEX runs_idempotency ON runs (org_id, idempotency_key) WHERE idempotency_key IS NOT NULL;
CREATE UNIQUE INDEX runs_one_active_per_subject ON runs (org_id, kind, cause_id)
  WHERE kind IN ('diagnose','fix') AND status NOT IN ('succeeded','failed','timed_out','cancelled','errored');
CREATE UNIQUE INDEX runs_repo_work_slot ON runs (org_id, repo_id)      -- v1: one work slot per repo
  WHERE runner_id IS NOT NULL AND slot_class = 'work';
CREATE INDEX runs_leasable  ON runs (priority DESC, created_at) WHERE status = 'queued' AND source_ready;
CREATE INDEX runs_await_src ON runs (org_id, repo_id, sha) WHERE status = 'queued' AND NOT source_ready;
CREATE INDEX runs_lease_exp ON runs (lease_until) WHERE runner_id IS NOT NULL;
CREATE INDEX runs_held_by   ON runs (runner_id) WHERE runner_id IS NOT NULL;
CREATE INDEX runs_ticket    ON runs (org_id, ticket_id, kind, created_at DESC);
CREATE INDEX runs_cause     ON runs (org_id, cause_id, created_at DESC);
CREATE INDEX runs_parent    ON runs (org_id, parent_run_id);
SELECT mage_tenant_table('runs');

CREATE TABLE run_attempts (
  org_id uuid NOT NULL, run_id uuid NOT NULL, attempt smallint NOT NULL,
  runner_id uuid NOT NULL REFERENCES runners (id),
  leased_at timestamptz NOT NULL, ended_at timestamptz NULL, end_reason text NULL,
  PRIMARY KEY (org_id, run_id, attempt),
  FOREIGN KEY (org_id, run_id) REFERENCES runs (org_id, id)
);
SELECT mage_tenant_table('run_attempts');

CREATE TABLE run_events (
  org_id uuid NOT NULL, run_id uuid NOT NULL, seq bigint NOT NULL,   -- from runs.next_seq in the same tx
  attempt smallint NOT NULL, at timestamptz NOT NULL,
  kind jsonb NOT NULL,                                    -- RunEventKind (previews <= 4 KiB)
  PRIMARY KEY (org_id, run_id, seq),
  FOREIGN KEY (org_id, run_id) REFERENCES runs (org_id, id)
);
SELECT mage_tenant_table('run_events');

CREATE TABLE run_commands (
  org_id uuid NOT NULL, run_id uuid NOT NULL, seq bigint NOT NULL,   -- from runs.next_cmd_seq
  attempt smallint NOT NULL, issued_at timestamptz NOT NULL,
  command jsonb NOT NULL,                                 -- Steer | Abort | GateFeedback
  PRIMARY KEY (org_id, run_id, seq),
  FOREIGN KEY (org_id, run_id) REFERENCES runs (org_id, id)
);
SELECT mage_tenant_table('run_commands');

CREATE TABLE gates (
  org_id uuid NOT NULL, id uuid NOT NULL, run_id uuid NOT NULL,
  kind    text NOT NULL CHECK (kind IN ('evidence','boot','repro','fix')),
  attempt smallint NOT NULL,
  status  text NOT NULL CHECK (status IN ('pending','passed','failed')),
  reason text NULL,
  feedback text NULL,                                     -- rendered GateFeedback details, reused as prior_feedback
  details jsonb NULL,                                     -- opaque, owned by the gate's module (e.g. EvidenceGateDetails)
  verify_run uuid NULL,
  created_at timestamptz NOT NULL, decided_at timestamptz NULL,
  PRIMARY KEY (org_id, id),
  UNIQUE (org_id, run_id, kind, attempt),
  FOREIGN KEY (org_id, run_id)     REFERENCES runs (org_id, id),
  FOREIGN KEY (org_id, verify_run) REFERENCES runs (org_id, id),
  CHECK ((status = 'pending') = (decided_at IS NULL))
);
SELECT mage_tenant_table('gates');

CREATE TABLE artifacts (
  org_id uuid NOT NULL, id uuid NOT NULL, run_id uuid NOT NULL, attempt smallint NOT NULL,
  kind text NOT NULL CHECK (kind IN ('video','gif','trace','screenshot','log','script','patch',
                                     'transcript','scip_index','fix_brief')),   -- source bundles live on repo_versions
  name text NOT NULL,
  key  text NOT NULL UNIQUE,                              -- org/{org}/runs/{run}/{kind}/{name}
  content_type text NOT NULL,
  bytes bigint NULL,                                      -- set on completion
  sha256 bytea NULL CHECK (octet_length(sha256) = 32),    -- set on completion
  state text NOT NULL CHECK (state IN ('pending','complete')),
  created_at timestamptz NOT NULL, completed_at timestamptz NULL,
  PRIMARY KEY (org_id, id),
  UNIQUE (org_id, run_id, attempt, kind, name),
  FOREIGN KEY (org_id, run_id) REFERENCES runs (org_id, id),
  CHECK (state = 'pending' OR (bytes IS NOT NULL AND sha256 IS NOT NULL))
);
CREATE INDEX artifacts_age ON artifacts (org_id, created_at);
SELECT mage_tenant_table('artifacts');

CREATE TABLE idempotency_replies (                        -- platform table (runs.md), TTL 24 h
  scope text NOT NULL, key text NOT NULL, org_id uuid NULL,
  request_hash bytea NOT NULL CHECK (octet_length(request_hash) = 32),
  response jsonb NOT NULL, created_at timestamptz NOT NULL,
  PRIMARY KEY (scope, key)
);
CREATE INDEX idempotency_replies_age ON idempotency_replies (created_at);
SELECT mage_platform_table('idempotency_replies');
-- RunStore writes the reply in the same tenant transaction as the run change it answers
GRANT SELECT, INSERT ON idempotency_replies TO mage_tenant;
