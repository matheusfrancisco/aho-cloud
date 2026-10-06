-- Core: orgs, users, settings, integrations, customers (storage.md "Schema").

-- platform tables -------------------------------------------------------------------------------

CREATE TABLE orgs (
  id                    uuid PRIMARY KEY,
  slug                  text NOT NULL UNIQUE,
  name                  text NOT NULL,
  state                 text NOT NULL CHECK (state IN ('active','deleting')),
  retention             jsonb NOT NULL,                 -- RetentionPolicy
  kek_name              text NULL,                      -- Cloud KMS CryptoKey resource; NULL in dev
  created_at            timestamptz NOT NULL,
  deletion_requested_at timestamptz NULL
);
SELECT mage_platform_table('orgs');
-- tenant-pool OrgStore methods: get_org, org_state, set_org_state, retention, set_retention
GRANT SELECT, UPDATE ON orgs TO mage_tenant;

CREATE TABLE users (
  id uuid PRIMARY KEY, email text NOT NULL, name text NOT NULL, created_at timestamptz NOT NULL
);
CREATE UNIQUE INDEX users_email ON users (lower(email));
SELECT mage_platform_table('users');
GRANT SELECT ON users TO mage_tenant;                    -- OrgStore::members joins users

CREATE TABLE user_identities (                            -- server.md: login identity key
  provider text NOT NULL, subject text NOT NULL,          -- ('github', numeric user id)
  user_id  uuid NOT NULL REFERENCES users (id),
  login    text NOT NULL, avatar_url text NULL,
  created_at timestamptz NOT NULL, updated_at timestamptz NOT NULL,
  PRIMARY KEY (provider, subject)
);
CREATE INDEX user_identities_user ON user_identities (user_id);
SELECT mage_platform_table('user_identities');
GRANT SELECT ON user_identities TO mage_tenant;          -- member listing shows the login

-- tenant tables ---------------------------------------------------------------------------------

CREATE TABLE memberships (
  org_id     uuid NOT NULL REFERENCES orgs (id),
  user_id    uuid NOT NULL REFERENCES users (id),
  role       text NOT NULL CHECK (role IN ('owner','admin','member')),
  created_at timestamptz NOT NULL,
  PRIMARY KEY (org_id, user_id)
);
CREATE INDEX memberships_user ON memberships (user_id);
SELECT mage_tenant_table('memberships');

CREATE TABLE invites (
  org_id uuid NOT NULL REFERENCES orgs (id), id uuid NOT NULL,
  provider text NOT NULL, subject text NOT NULL, login text NOT NULL,
  role text NOT NULL CHECK (role IN ('owner','admin','member')),
  invited_by uuid NOT NULL REFERENCES users (id),
  created_at timestamptz NOT NULL, expires_at timestamptz NOT NULL,   -- 14 days (server.md)
  PRIMARY KEY (org_id, id),
  UNIQUE (org_id, provider, subject)
);
CREATE INDEX invites_subject ON invites (provider, subject);
SELECT mage_tenant_table('invites');

CREATE TABLE org_settings (
  org_id uuid NOT NULL REFERENCES orgs (id),
  area   text NOT NULL CHECK (area IN ('general','model','triage','fix')),
  value  jsonb NOT NULL,                                  -- GeneralSettings | ModelSettings | TriageSettings | FixSettings
  updated_at timestamptz NOT NULL, updated_by text NOT NULL,          -- Actor
  PRIMARY KEY (org_id, area)
);
SELECT mage_tenant_table('org_settings');

CREATE TABLE integrations (
  org_id      uuid NOT NULL REFERENCES orgs (id),
  kind        text NOT NULL CHECK (kind IN ('github','github_fix','linear','slack')),
  external_id text NOT NULL,          -- installation id / Linear workspace id / Slack team id
  status      text NOT NULL CHECK (status IN ('active','revoked')),
  settings    jsonb NOT NULL DEFAULT '{}',                -- per-kind non-secret settings (intake.md)
  created_at  timestamptz NOT NULL,
  updated_at  timestamptz NOT NULL,
  PRIMARY KEY (org_id, kind, external_id)
);
-- one active owner per external account: webhook tenant resolution is unambiguous
CREATE UNIQUE INDEX integrations_active ON integrations (kind, external_id) WHERE status = 'active';
SELECT mage_tenant_table('integrations');

CREATE TABLE org_intake_config (
  org_id uuid PRIMARY KEY REFERENCES orgs (id), config jsonb NOT NULL, updated_at timestamptz NOT NULL
);
SELECT mage_tenant_table('org_intake_config');

CREATE TABLE customers (
  org_id uuid NOT NULL REFERENCES orgs (id), id uuid NOT NULL,
  name text NOT NULL, environment text NULL,
  created_at timestamptz NOT NULL, updated_at timestamptz NOT NULL,
  PRIMARY KEY (org_id, id)
);
SELECT mage_tenant_table('customers');

CREATE TABLE customer_refs (                              -- ExternalRef (intake.md variants)
  org_id uuid NOT NULL, customer_id uuid NOT NULL,
  system text NOT NULL CHECK (system IN ('linear_customer','linear_label','slack_channel')),
  value  text NOT NULL,                                   -- slack_channel: "{team}/{channel}"
  PRIMARY KEY (org_id, system, value),                    -- a ref names at most one customer
  FOREIGN KEY (org_id, customer_id) REFERENCES customers (org_id, id)
);
CREATE INDEX customer_refs_customer ON customer_refs (org_id, customer_id);
SELECT mage_tenant_table('customer_refs');
