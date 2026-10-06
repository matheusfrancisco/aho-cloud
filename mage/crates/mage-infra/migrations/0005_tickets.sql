-- Core: tickets, causes, fixes, resolutions, outbound (storage.md "Schema"; TicketStore,
-- OutboundLog). Closes the runs <-> tickets / causes cycle at the end.

CREATE TABLE tickets (
  org_id uuid NOT NULL REFERENCES orgs (id), id uuid NOT NULL,
  source            text NOT NULL CHECK (source IN ('linear')),
  source_issue_id   text NOT NULL, source_identifier text NOT NULL, source_url text NOT NULL,
  title text NOT NULL, body text NOT NULL,
  content_hash bytea NOT NULL CHECK (octet_length(content_hash) = 32),
  triaged_hash bytea NULL CHECK (octet_length(triaged_hash) = 32),   -- written by begin_triage (intake.md, triage.md)
  triage_run   uuid NULL,                                 -- fencing token: current triage run
  slack_team text NULL, slack_channel text NULL, slack_ts text NULL,
  customer_id uuid NULL,
  target_repo_id uuid NULL,
  target_sha bytea NULL CHECK (octet_length(target_sha) = 20),
  target_origin text NULL CHECK (target_origin IN ('override','directive','deployment','default_head')),
  target_environment text NULL,                           -- TargetOrigin::Deployment { environment }
  error_strings text[] NOT NULL DEFAULT '{}',
  status text NOT NULL CHECK (status IN ('new','triaging','triaged','triage_failed','diagnosing',
                                         'diagnosed','fixing','fix_ready','pr_open','resolved','closed')),
  rev    bigint NOT NULL,
  search tsvector GENERATED ALWAYS AS (to_tsvector('simple', title || ' ' || body)) STORED,
  created_at timestamptz NOT NULL, updated_at timestamptz NOT NULL,
  PRIMARY KEY (org_id, id),
  UNIQUE (org_id, source, source_issue_id),
  FOREIGN KEY (org_id, customer_id)    REFERENCES customers (org_id, id),
  FOREIGN KEY (org_id, target_repo_id) REFERENCES repos (org_id, id),
  CHECK ((target_repo_id IS NULL) = (target_sha IS NULL) AND (target_sha IS NULL) = (target_origin IS NULL))
);
CREATE INDEX tickets_status  ON tickets (org_id, status, updated_at DESC);
CREATE INDEX tickets_assumed ON tickets (org_id, target_repo_id)
  WHERE target_origin = 'default_head' AND status NOT IN ('resolved','closed');    -- list_open_assumed
CREATE INDEX tickets_search  ON tickets USING gin (search);
SELECT mage_tenant_table('tickets');

CREATE TABLE ticket_thread_snapshots (                    -- intake.md ThreadSnapshot
  org_id uuid NOT NULL, ticket_id uuid NOT NULL,
  snapshot jsonb NOT NULL, fetched_at timestamptz NOT NULL,
  PRIMARY KEY (org_id, ticket_id),
  FOREIGN KEY (org_id, ticket_id) REFERENCES tickets (org_id, id)
);
SELECT mage_tenant_table('ticket_thread_snapshots');

CREATE TABLE causes (
  org_id uuid NOT NULL, id uuid NOT NULL, ticket_id uuid NOT NULL,
  triage_run   uuid NOT NULL,
  rank smallint NOT NULL, title text NOT NULL, explanation text NOT NULL,
  repro_steps  text[] NOT NULL, confirm_if text NOT NULL, rule_out_if text NOT NULL,
  confidence   real NOT NULL CHECK (confidence BETWEEN 0 AND 1),
  method       text NOT NULL CHECK (method IN ('ui','api','job','unknown')),
  status       text NOT NULL CHECK (status IN ('untested','running','reproduced','not_reproduced','inconclusive')),
  status_run   uuid NULL, status_gate uuid NULL, status_reason text NULL,
  superseded_at timestamptz NULL,
  created_at timestamptz NOT NULL, updated_at timestamptz NOT NULL,
  PRIMARY KEY (org_id, id),
  FOREIGN KEY (org_id, ticket_id)   REFERENCES tickets (org_id, id),
  FOREIGN KEY (org_id, triage_run)  REFERENCES runs (org_id, id)  DEFERRABLE INITIALLY IMMEDIATE,
  FOREIGN KEY (org_id, status_run)  REFERENCES runs (org_id, id)  DEFERRABLE INITIALLY IMMEDIATE,
  FOREIGN KEY (org_id, status_gate) REFERENCES gates (org_id, id) DEFERRABLE INITIALLY IMMEDIATE,
  CHECK ((status = 'untested') = (status_run IS NULL)),
  CHECK (status <> 'reproduced' OR status_gate IS NOT NULL)      -- mirrors ReproGatePassed
);
CREATE INDEX causes_ticket ON causes (org_id, ticket_id, rank) WHERE superseded_at IS NULL;
SELECT mage_tenant_table('causes');

CREATE TABLE cause_citations (
  org_id uuid NOT NULL, cause_id uuid NOT NULL, ord smallint NOT NULL,
  repo_id uuid NOT NULL,
  sha bytea NOT NULL CHECK (octet_length(sha) = 20),
  path text NOT NULL,
  line_start int NOT NULL CHECK (line_start >= 1),
  line_end   int NOT NULL CHECK (line_end >= line_start),
  snippet_hash bytea NULL CHECK (octet_length(snippet_hash) = 32),   -- triage.md rule
  note         text NULL CHECK (char_length(note) <= 200),
  PRIMARY KEY (org_id, cause_id, ord),
  FOREIGN KEY (org_id, cause_id) REFERENCES causes (org_id, id),
  FOREIGN KEY (org_id, repo_id)  REFERENCES repos (org_id, id)      -- not repo_versions: version GC must not block
);
SELECT mage_tenant_table('cause_citations');

CREATE TABLE fix_briefs (                                 -- fix.md
  org_id uuid NOT NULL, id uuid NOT NULL, cause_id uuid NOT NULL,
  diagnose_run_id uuid NOT NULL, artifact_id uuid NOT NULL,
  version int NOT NULL, markdown text NOT NULL, created_at timestamptz NOT NULL,
  PRIMARY KEY (org_id, id),
  UNIQUE (org_id, cause_id, version),
  FOREIGN KEY (org_id, cause_id)        REFERENCES causes (org_id, id),
  FOREIGN KEY (org_id, diagnose_run_id) REFERENCES runs (org_id, id),
  FOREIGN KEY (org_id, artifact_id)     REFERENCES artifacts (org_id, id)
);
SELECT mage_tenant_table('fix_briefs');

CREATE TABLE fix_pull_requests (                          -- fix.md
  org_id uuid NOT NULL, id uuid NOT NULL, ticket_id uuid NOT NULL,
  fix_run_id uuid NOT NULL, gate_id uuid NOT NULL,
  changeset_digest bytea NOT NULL CHECK (octet_length(changeset_digest) = 32),
  branch text NOT NULL,
  base_sha bytea NOT NULL CHECK (octet_length(base_sha) = 20),
  head_commit bytea NULL CHECK (octet_length(head_commit) = 20),
  pr_number bigint NULL, pr_url text NULL,
  state  text NOT NULL CHECK (state IN ('planned','commit_created','branch_created','opened','closed')),
  merged boolean NULL,                                    -- FixPrState::Closed { merged }
  created_at timestamptz NOT NULL, updated_at timestamptz NOT NULL,
  PRIMARY KEY (org_id, id),
  UNIQUE (org_id, ticket_id, changeset_digest),
  FOREIGN KEY (org_id, ticket_id)  REFERENCES tickets (org_id, id),
  FOREIGN KEY (org_id, fix_run_id) REFERENCES runs (org_id, id),
  FOREIGN KEY (org_id, gate_id)    REFERENCES gates (org_id, id),
  CHECK ((state = 'closed') = (merged IS NOT NULL))
);
CREATE INDEX fix_prs_number ON fix_pull_requests (org_id, pr_number) WHERE pr_number IS NOT NULL;
-- pull_request_by_number joins runs on fix_run_id to filter repo_id
SELECT mage_tenant_table('fix_pull_requests');

CREATE TABLE resolutions (
  org_id uuid NOT NULL, ticket_id uuid NOT NULL,
  confirmed_cause uuid NULL, root_cause text NOT NULL, fix_pr_url text NULL,
  repro_script uuid NULL,                                 -- artifact id; pinned by retention
  resolved_by uuid NOT NULL REFERENCES users (id), at timestamptz NOT NULL,
  search tsvector GENERATED ALWAYS AS (to_tsvector('simple', root_cause)) STORED,
  PRIMARY KEY (org_id, ticket_id),                        -- Resolved is terminal: one per ticket
  FOREIGN KEY (org_id, ticket_id)       REFERENCES tickets (org_id, id),
  FOREIGN KEY (org_id, confirmed_cause) REFERENCES causes (org_id, id),
  FOREIGN KEY (org_id, repro_script)    REFERENCES artifacts (org_id, id)
);
CREATE INDEX resolutions_search ON resolutions USING gin (search);
SELECT mage_tenant_table('resolutions');

CREATE TABLE outbound_messages (                          -- intake.md (OutboundLog)
  org_id uuid NOT NULL, id uuid NOT NULL, ticket_id uuid NOT NULL,
  target      text NOT NULL CHECK (target IN ('linear_comment','slack_reply')),
  notice_key  text NOT NULL,
  client_id   uuid NOT NULL,                              -- pre-allocated Linear comment id, reused on retry
  state       text NOT NULL CHECK (state IN ('pending','sent','failed','skipped')),
  uncertain   boolean NOT NULL DEFAULT false,
  external_id text NULL,
  body_sha256 bytea NOT NULL CHECK (octet_length(body_sha256) = 32),
  attempts smallint NOT NULL DEFAULT 0, last_error text NULL,
  created_at timestamptz NOT NULL, sent_at timestamptz NULL,
  PRIMARY KEY (org_id, id),
  UNIQUE (org_id, ticket_id, target, notice_key),
  FOREIGN KEY (org_id, ticket_id) REFERENCES tickets (org_id, id)
);
CREATE INDEX outbound_external ON outbound_messages (org_id, target, external_id) WHERE external_id IS NOT NULL;
SELECT mage_tenant_table('outbound_messages');

-- runs -> tickets / causes: deferrable so org deletion can remove the cause/run/gate cycle in one
-- transaction (storage.md "Conventions").
ALTER TABLE runs
  ADD CONSTRAINT runs_ticket_fkey FOREIGN KEY (org_id, ticket_id) REFERENCES tickets (org_id, id)
    DEFERRABLE INITIALLY IMMEDIATE,
  ADD CONSTRAINT runs_cause_fkey  FOREIGN KEY (org_id, cause_id)  REFERENCES causes (org_id, id)
    DEFERRABLE INITIALLY IMMEDIATE;
