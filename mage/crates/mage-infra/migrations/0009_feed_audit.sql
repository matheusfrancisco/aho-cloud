-- Ops: per-org change feed (ChangeFeed, server.md) and audit log (AuditLog, server.md)
-- (storage.md "Ops", "NOTIFY channels").

-- feed ids come from a per-org counter row in the writing transaction, not a sequence: commit
-- order equals id order, so a reader resuming at an id never skips one.
CREATE TABLE org_feed_heads (org_id uuid PRIMARY KEY REFERENCES orgs (id), next_id bigint NOT NULL);
SELECT mage_tenant_table('org_feed_heads');

CREATE TABLE org_feed (                                   -- server.md (ChangeFeed)
  org_id uuid NOT NULL, id bigint NOT NULL,               -- from org_feed_heads in the writing tx
  kind text NOT NULL, subject text NOT NULL, at timestamptz NOT NULL,
  PRIMARY KEY (org_id, id)
);
CREATE INDEX org_feed_age ON org_feed (at);
SELECT mage_tenant_table('org_feed');

-- NOTIFY mage_org_feed 'org_id:feed_id' on every feed insert; delivered at commit, dropped on
-- rollback. Every store method that writes the feed gets the wakeup without its own pg_notify.
CREATE FUNCTION mage_org_feed_notify() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  PERFORM pg_notify('mage_org_feed', NEW.org_id::text || ':' || NEW.id::text);
  RETURN NULL;
END
$$;
CREATE TRIGGER org_feed_notify AFTER INSERT ON org_feed
  FOR EACH ROW EXECUTE FUNCTION mage_org_feed_notify();

CREATE TABLE audit_log (                                  -- server.md (AuditLog)
  id bigserial PRIMARY KEY, org_id uuid NULL,
  actor_kind text NOT NULL, actor_id text NULL, action text NOT NULL,
  target_kind text NULL, target_id text NULL, ip inet NULL, request_id text NULL,
  at timestamptz NOT NULL, details jsonb NOT NULL DEFAULT '{}'
);
CREATE INDEX audit_log_org ON audit_log (org_id, at DESC);
-- append-only: no role may UPDATE; only the platform role deletes (ops.purge past retention)
GRANT SELECT, INSERT ON audit_log TO mage_tenant, mage_platform;
GRANT DELETE ON audit_log TO mage_platform;
GRANT USAGE ON SEQUENCE audit_log_id_seq TO mage_tenant, mage_platform;
