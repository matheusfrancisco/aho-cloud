-- Core: repos, versions, layers, recipes, deployments (storage.md "Schema"; RepoStore).
-- Git ids are 20-byte bytea, hashes 32-byte bytea (storage.md "Conventions").

CREATE TABLE repos (
  org_id uuid NOT NULL REFERENCES orgs (id), id uuid NOT NULL,
  external_id     bigint NOT NULL,                        -- GitHub repository id, stable across renames
  owner text NOT NULL, name text NOT NULL, default_branch text NOT NULL,
  installation_id bigint NOT NULL,
  access text NOT NULL CHECK (access IN ('granted','lost')),          -- RepoAccess (intake.md)
  created_at timestamptz NOT NULL,
  PRIMARY KEY (org_id, id),
  UNIQUE (org_id, external_id)
);
CREATE INDEX repos_name ON repos (org_id, lower(owner), lower(name));
SELECT mage_tenant_table('repos');

CREATE TABLE repo_versions (
  org_id uuid NOT NULL, repo_id uuid NOT NULL,
  sha           bytea NOT NULL CHECK (octet_length(sha) = 20),
  tree_oid      bytea NULL CHECK (octet_length(tree_oid) = 20),   -- set by index.manifest / index.bundle_source
  revs          jsonb NULL,                               -- IndexRevs {parser, errors, appmap} of the Ready layers
  bundle_key    text NULL, bundle_bytes bigint NULL,      -- SourceBundleRef (tree = tree_oid)
  bundle_sha256 bytea NULL CHECK (octet_length(bundle_sha256) = 32),
  created_at    timestamptz NOT NULL,
  PRIMARY KEY (org_id, repo_id, sha),
  FOREIGN KEY (org_id, repo_id) REFERENCES repos (org_id, id),
  CHECK ((bundle_key IS NULL) = (bundle_sha256 IS NULL))
);
CREATE INDEX repo_versions_bundle_tree ON repo_versions (org_id, repo_id, tree_oid) WHERE bundle_key IS NOT NULL;
SELECT mage_tenant_table('repo_versions');

CREATE TABLE repo_version_reasons (
  org_id uuid NOT NULL, repo_id uuid NOT NULL, sha bytea NOT NULL,
  reason   text NOT NULL CHECK (reason IN ('default_head','release','deployed','on_demand')),
  detail   text NOT NULL DEFAULT '',                      -- tag / environment / ticket id
  added_at timestamptz NOT NULL,
  ended_at timestamptz NULL,                              -- singleton reason moved to another sha
  PRIMARY KEY (org_id, repo_id, sha, reason, detail, added_at),
  FOREIGN KEY (org_id, repo_id, sha) REFERENCES repo_versions (org_id, repo_id, sha)
);
CREATE UNIQUE INDEX reasons_current     ON repo_version_reasons (org_id, repo_id, sha, reason, detail) WHERE ended_at IS NULL;
CREATE UNIQUE INDEX reasons_head_single ON repo_version_reasons (org_id, repo_id) WHERE reason = 'default_head' AND ended_at IS NULL;
CREATE UNIQUE INDEX reasons_env_single  ON repo_version_reasons (org_id, repo_id, detail) WHERE reason = 'deployed' AND ended_at IS NULL;
SELECT mage_tenant_table('repo_version_reasons');

CREATE TABLE layer_status (
  org_id uuid NOT NULL, repo_id uuid NOT NULL, sha bytea NOT NULL,
  layer  text NOT NULL CHECK (layer IN ('files','symbols','refs','error_sites','app_map','semantic','history')),
  state  text NOT NULL CHECK (state IN ('pending','running','ready','failed','skipped')),
  reason text NULL,                                       -- Failed / Skipped
  ready_at timestamptz NULL, updated_at timestamptz NOT NULL,
  PRIMARY KEY (org_id, repo_id, sha, layer),
  FOREIGN KEY (org_id, repo_id, sha) REFERENCES repo_versions (org_id, repo_id, sha)
) WITH (fillfactor = 80, autovacuum_vacuum_scale_factor = 0.01);   -- high churn (storage.md "Indexes")
SELECT mage_tenant_table('layer_status');

CREATE TABLE layer_batches (                              -- fan-in for batched layers (knowledge.md)
  org_id uuid NOT NULL, repo_id uuid NOT NULL, sha bytea NOT NULL,
  layer text NOT NULL, batch int NOT NULL, done_at timestamptz NULL,
  PRIMARY KEY (org_id, repo_id, sha, layer, batch),
  FOREIGN KEY (org_id, repo_id, sha) REFERENCES repo_versions (org_id, repo_id, sha)
);
SELECT mage_tenant_table('layer_batches');

CREATE TABLE recipes (
  org_id uuid NOT NULL, repo_id uuid NOT NULL,
  hash             bytea NOT NULL CHECK (octet_length(hash) = 32),   -- RecipeHash
  body             text NOT NULL,                         -- normalized TOML
  status           text NOT NULL CHECK (status IN ('draft','validated','broken')),
  validated_at_sha bytea NULL CHECK (octet_length(validated_at_sha) = 20),
  validated_at     timestamptz NULL,
  broken_reason    text NULL, broken_at timestamptz NULL,
  created_at       timestamptz NOT NULL,
  PRIMARY KEY (org_id, repo_id, hash),
  FOREIGN KEY (org_id, repo_id) REFERENCES repos (org_id, id)
);
CREATE INDEX recipes_validated ON recipes (org_id, repo_id, validated_at DESC) WHERE status = 'validated';
CREATE INDEX recipes_recent    ON recipes (org_id, repo_id, created_at DESC);
SELECT mage_tenant_table('recipes');

CREATE TABLE deployments (                                -- append-only history
  org_id uuid NOT NULL, repo_id uuid NOT NULL, environment text NOT NULL,
  observed_at timestamptz NOT NULL,
  sha bytea NOT NULL CHECK (octet_length(sha) = 20),
  source text NOT NULL CHECK (source IN ('github_deployments','manual','tag')),
  PRIMARY KEY (org_id, repo_id, environment, observed_at),   -- latest_deployment = backward PK scan
  FOREIGN KEY (org_id, repo_id) REFERENCES repos (org_id, id)
);
SELECT mage_tenant_table('deployments');
