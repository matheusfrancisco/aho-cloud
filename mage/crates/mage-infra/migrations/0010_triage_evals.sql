-- Ops: triage evaluation harness (TriageEvalStore, triage.md) (storage.md "Ops").

CREATE TABLE triage_evals (                               -- triage.md (TriageEvalStore)
  org_id uuid NOT NULL REFERENCES orgs (id), id uuid NOT NULL,
  name text NOT NULL, corpus text NOT NULL, variant_toml text NOT NULL, repeat smallint NOT NULL,
  created_by uuid NOT NULL REFERENCES users (id), created_at timestamptz NOT NULL,
  PRIMARY KEY (org_id, id)
);
SELECT mage_tenant_table('triage_evals');

CREATE TABLE triage_eval_cases (
  org_id uuid NOT NULL, eval_id uuid NOT NULL, ticket_id uuid NOT NULL, repeat smallint NOT NULL,
  input_run uuid NULL, run_id uuid NULL, status text NULL,
  hit_rank smallint NULL, primary_hit_rank smallint NULL, causes smallint NOT NULL,
  usage jsonb NULL, queue_wait_ms bigint NULL, agent_time_ms bigint NULL,
  gate_attempts smallint NOT NULL, dropped_causes smallint NOT NULL, adjusted_citations smallint NOT NULL,
  finished_at timestamptz NULL,
  PRIMARY KEY (org_id, eval_id, ticket_id, repeat),
  FOREIGN KEY (org_id, eval_id)   REFERENCES triage_evals (org_id, id),
  FOREIGN KEY (org_id, ticket_id) REFERENCES tickets (org_id, id),
  FOREIGN KEY (org_id, run_id)    REFERENCES runs (org_id, id)
);
SELECT mage_tenant_table('triage_eval_cases');

CREATE TABLE triage_labels (
  org_id uuid NOT NULL, ticket_id uuid NOT NULL,
  paths text[] NOT NULL,
  strength text NOT NULL CHECK (strength IN ('strong','medium')),
  source   text NOT NULL CHECK (source IN ('fix_pr','confirmed_cause','manual')),
  labeled_by uuid NULL REFERENCES users (id), updated_at timestamptz NOT NULL,
  PRIMARY KEY (org_id, ticket_id),
  FOREIGN KEY (org_id, ticket_id) REFERENCES tickets (org_id, id)
);
SELECT mage_tenant_table('triage_labels');
