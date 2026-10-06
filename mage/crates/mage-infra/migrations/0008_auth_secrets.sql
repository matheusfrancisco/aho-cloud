-- Ops: sessions, one-time states, runner bootstrap tokens, revocations (TokenStore) and sealed
-- secrets / org data keys (SecretStore) (storage.md "Ops", "Encryption").

CREATE TABLE sessions (
  id_hash bytea PRIMARY KEY CHECK (octet_length(id_hash) = 32),
  user_id uuid NOT NULL REFERENCES users (id),
  csrf_secret bytea NOT NULL,
  created_at timestamptz NOT NULL, last_seen_at timestamptz NOT NULL, expires_at timestamptz NOT NULL,
  ip inet NULL, user_agent text NULL
);
CREATE INDEX sessions_user ON sessions (user_id);
CREATE INDEX sessions_exp  ON sessions (expires_at);
SELECT mage_platform_table('sessions');

CREATE TABLE oauth_states (                               -- login and install states, one-time
  state_hash bytea PRIMARY KEY CHECK (octet_length(state_hash) = 32),
  purpose  text NOT NULL CHECK (purpose IN ('login','install')),
  org_id   uuid NULL REFERENCES orgs (id), provider text NULL,          -- install only
  user_id  uuid NULL REFERENCES users (id),
  pkce_verifier text NULL, return_to text NULL,
  expires_at timestamptz NOT NULL, created_at timestamptz NOT NULL,
  CHECK ((purpose = 'install') = (org_id IS NOT NULL AND provider IS NOT NULL))
);
CREATE INDEX oauth_states_exp ON oauth_states (expires_at);
SELECT mage_platform_table('oauth_states');

CREATE TABLE runner_bootstrap_tokens (
  token_hash bytea PRIMARY KEY CHECK (octet_length(token_hash) = 32),
  name  text NOT NULL,                                    -- the one runner name this token may register
  scope jsonb NOT NULL,                                   -- RunnerScope, copied to runners.scope
  expires_at timestamptz NOT NULL, used_at timestamptz NULL,
  runner_id uuid NULL REFERENCES runners (id),
  created_by text NOT NULL, created_at timestamptz NOT NULL           -- Actor (operator CLI)
);
SELECT mage_platform_table('runner_bootstrap_tokens');

CREATE TABLE token_revocations (
  jti bytea PRIMARY KEY CHECK (octet_length(jti) = 16),
  org_id uuid NULL, reason text NOT NULL,
  revoked_at timestamptz NOT NULL, expires_at timestamptz NOT NULL     -- deleted after the token's exp
);
CREATE INDEX token_revocations_exp ON token_revocations (expires_at);
SELECT mage_platform_table('token_revocations');

CREATE TABLE secrets (
  org_id  uuid NULL REFERENCES orgs (id),                 -- NULL = platform scope
  name    text NOT NULL, version int NOT NULL,
  state   text NOT NULL CHECK (state IN ('staged','active','retired','destroyed')),
  wrapped_dek bytea NULL, kek_version text NOT NULL,
  nonce bytea NOT NULL CHECK (octet_length(nonce) = 12),  -- AES-256-GCM 96-bit nonce
  ciphertext  bytea NULL,
  fingerprint text NOT NULL, created_by text NOT NULL,    -- Actor
  created_at timestamptz NOT NULL, state_changed_at timestamptz NOT NULL,
  CHECK ((state = 'destroyed') = (ciphertext IS NULL)),
  CHECK ((state = 'destroyed') = (wrapped_dek IS NULL))
);
CREATE UNIQUE INDEX secrets_version ON secrets (org_id, name, version) NULLS NOT DISTINCT;
CREATE UNIQUE INDEX secrets_active  ON secrets (org_id, name) NULLS NOT DISTINCT WHERE state = 'active';
SELECT mage_platform_table('secrets');
-- org-scope SecretStore methods run on the tenant pool; rows are sealed (SecretVault)
GRANT SELECT, INSERT, UPDATE, DELETE ON secrets TO mage_tenant;

CREATE TABLE data_keys (
  org_id uuid NOT NULL REFERENCES orgs (id), version int NOT NULL,
  wrapped_dek bytea NULL, kek_version text NOT NULL,
  state text NOT NULL CHECK (state IN ('active','retired','destroyed')),
  created_at timestamptz NOT NULL,
  PRIMARY KEY (org_id, version)
);
CREATE UNIQUE INDEX data_keys_active ON data_keys (org_id) WHERE state = 'active';
SELECT mage_tenant_table('data_keys');
