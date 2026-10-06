-- Foundation: extensions, tenancy function, group roles, tenancy helpers (storage.md "Migrations",
-- "Tenancy enforcement").
--
-- Requirements: PostgreSQL >= 16 baseline (`NULLS NOT DISTINCT` needs >= 15). The migrating role
-- (`mage_owner`) owns the schema and needs CREATEROLE the first time (group roles are created if
-- absent) and the right to create extensions (`cloudsqlsuperuser` on Cloud SQL).
--
-- Extensions:
--   pg_trgm  trigram GIN index `symbols_name_trgm` (fuzzy `find_symbol`, knowledge.md).
-- Not created in v1:
--   vector   (pgvector) is only used by knowledge layer 5 (`chunks` embeddings), which is not part
--            of v1; the migration that adds `chunks` creates it.
--   btree_gin is an at-scale option for composite GIN indexes; no v1 index uses it.
--
-- Roles: login roles (Cloud SQL IAM users) are provisioned outside SQL and granted membership in
-- one of the two group roles below. Every tenant table (org_id NOT NULL) gets row-level security
-- with one policy per group via `mage_tenant_table`; platform tables get no RLS and are granted to
-- `mage_platform` via `mage_platform_table`. Extra grants of platform tables to `mage_tenant`
-- (outbox insert into `jobs`, tenant-scoped reads of `orgs` / `users`, org-scoped secrets, audit
-- writes) are listed next to each table.

CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- Tenant of the current transaction; set by `with_org_tx` through
-- `SELECT set_config('mage.org_id', $org, true)`. Unset → NULL → every tenant policy matches no row.
CREATE FUNCTION current_org() RETURNS uuid LANGUAGE sql STABLE
  AS $$ SELECT NULLIF(current_setting('mage.org_id', true), '')::uuid $$;

DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mage_tenant') THEN
    BEGIN
      CREATE ROLE mage_tenant NOLOGIN;
    EXCEPTION WHEN duplicate_object OR unique_violation THEN
      NULL;  -- created concurrently by a migration of another database in the same cluster
    END;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mage_platform') THEN
    BEGIN
      CREATE ROLE mage_platform NOLOGIN;
    EXCEPTION WHEN duplicate_object OR unique_violation THEN
      NULL;
    END;
  END IF;
END
$$;

GRANT USAGE ON SCHEMA public TO mage_tenant, mage_platform;

-- Tenant table: RLS on, tenant policy bound to current_org(), platform policy unrestricted, DML for
-- both groups. Called right after each tenant CREATE TABLE so RLS is never missing.
CREATE FUNCTION mage_tenant_table(t regclass) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
  rel text := (SELECT relname FROM pg_class WHERE oid = t);
BEGIN
  EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', t);
  EXECUTE format(
    'CREATE POLICY %I ON %s TO mage_tenant USING (org_id = current_org()) WITH CHECK (org_id = current_org())',
    rel || '_tenant', t);
  EXECUTE format('CREATE POLICY %I ON %s TO mage_platform USING (true) WITH CHECK (true)',
    rel || '_platform', t);
  EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %s TO mage_tenant, mage_platform', t);
END
$$;

-- Platform table: no RLS, DML for the platform group only.
CREATE FUNCTION mage_platform_table(t regclass) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %s TO mage_platform', t);
END
$$;
