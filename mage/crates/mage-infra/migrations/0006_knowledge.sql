-- Knowledge facts (storage.md "Knowledge"; KnowledgeStore, knowledge.md). Keys, indexes and tenancy
-- are storage.md's; non-key columns follow knowledge.md where the two differ:
--   blobs.line_count NOT NULL (BlobMeta.line_count: u32, 0 for non-text);
--   symbols gain qualified, name_line, name_col, exported; container is the enclosing SymbolDef ord;
--   name_refs gain qualifier; its enclosing ord column is `enclosing` (knowledge.md call_edges SQL);
--   commits carry CommitInfo (tree_oid, author_time, committer_time; no author e-mail is extracted);
--   commit_files gain adds / dels (FileChange);
--   scip_symbols gain relationships (ScipSymbolInfo); scip_occurrences gain enclosing_range.
-- Layer 5 (Semantic: chunks, summaries, glossary_terms; pgvector) is not part of v1 and has no
-- tables here.

-- blob-keyed (no FKs: high-volume ingest; integrity by ingest order) ----------------------------

CREATE TABLE blobs (
  org_id uuid NOT NULL,
  blob_oid bytea NOT NULL CHECK (octet_length(blob_oid) = 20),
  size bigint NOT NULL, line_count int NOT NULL, is_text boolean NOT NULL,
  content_stored boolean NOT NULL,                        -- raw bytes at org/{org}/blobs/{aa}/{oid}
  created_at timestamptz NOT NULL,                        -- set by put_blob_meta; GC grace window
  PRIMARY KEY (org_id, blob_oid)
);
SELECT mage_tenant_table('blobs');

CREATE TABLE blob_stages (
  org_id uuid NOT NULL, blob_oid bytea NOT NULL CHECK (octet_length(blob_oid) = 20),
  stage  text NOT NULL CHECK (stage IN ('parse','error_sites','app_facts','chunks')),
  rev    smallint NOT NULL,
  lang   text NULL, status text NOT NULL CHECK (status IN ('ok','partial','failed')),
  error  text NULL, at timestamptz NOT NULL,
  PRIMARY KEY (org_id, blob_oid, stage, rev)
);
SELECT mage_tenant_table('blob_stages');

CREATE TABLE symbols (
  org_id uuid NOT NULL, blob_oid bytea NOT NULL CHECK (octet_length(blob_oid) = 20),
  parser_rev smallint NOT NULL, ord int NOT NULL,
  name text NOT NULL, qualified text NOT NULL, kind text NOT NULL,
  container int NULL,                                     -- ord of the enclosing SymbolDef
  start_line int NOT NULL, end_line int NOT NULL,
  name_line int NOT NULL, name_col int NOT NULL,
  signature text NULL, doc text NULL,
  exported boolean NOT NULL,
  PRIMARY KEY (org_id, blob_oid, parser_rev, ord)
);
CREATE INDEX symbols_name      ON symbols (org_id, name);
CREATE INDEX symbols_name_trgm ON symbols USING gin (name gin_trgm_ops);    -- fuzzy find_symbol
SELECT mage_tenant_table('symbols');

CREATE TABLE name_refs (
  org_id uuid NOT NULL, blob_oid bytea NOT NULL CHECK (octet_length(blob_oid) = 20),
  parser_rev smallint NOT NULL, ord int NOT NULL,
  name text NOT NULL, qualifier text NULL, kind text NOT NULL,
  line int NOT NULL, col int NOT NULL,
  enclosing int NULL,                                     -- ord of the enclosing SymbolDef
  PRIMARY KEY (org_id, blob_oid, parser_rev, ord)
);
CREATE INDEX name_refs_name ON name_refs (org_id, name);
SELECT mage_tenant_table('name_refs');

CREATE TABLE literal_sites (
  org_id uuid NOT NULL, blob_oid bytea NOT NULL CHECK (octet_length(blob_oid) = 20),
  parser_rev smallint NOT NULL, ord int NOT NULL,
  parts jsonb NOT NULL, context jsonb NOT NULL, line int NOT NULL, col int NOT NULL, enclosing_ord int NULL,
  PRIMARY KEY (org_id, blob_oid, parser_rev, ord)
);
SELECT mage_tenant_table('literal_sites');

CREATE TABLE error_sites (
  org_id uuid NOT NULL, blob_oid bytea NOT NULL CHECK (octet_length(blob_oid) = 20),
  errors_rev smallint NOT NULL, ord int NOT NULL,
  literal_ord int NOT NULL,
  kind text NOT NULL CHECK (kind IN ('throw','log','http_response','ui_message','i18n_use')),
  level text NULL, template text NOT NULL, pattern text NOT NULL,
  tokens text[] NOT NULL,                                 -- lowercased literal words, sorted, deduped
  specificity int NOT NULL, line int NOT NULL, col int NOT NULL, enclosing_ord int NULL,
  http_status smallint NULL, i18n_key text NULL,
  PRIMARY KEY (org_id, blob_oid, errors_rev, ord)
);
CREATE INDEX error_sites_tokens ON error_sites USING gin (tokens);         -- array_ops: tokens <@ $input
SELECT mage_tenant_table('error_sites');

CREATE TABLE catalog_entries (
  org_id uuid NOT NULL, blob_oid bytea NOT NULL CHECK (octet_length(blob_oid) = 20),
  errors_rev smallint NOT NULL, key text NOT NULL,
  locale text NULL, template text NOT NULL, pattern text NOT NULL,
  tokens text[] NOT NULL, specificity int NOT NULL, line int NOT NULL,
  PRIMARY KEY (org_id, blob_oid, errors_rev, key)
);
CREATE INDEX catalog_entries_tokens ON catalog_entries USING gin (tokens);
SELECT mage_tenant_table('catalog_entries');

CREATE TABLE app_facts (
  org_id uuid NOT NULL, blob_oid bytea NOT NULL CHECK (octet_length(blob_oid) = 20),
  appmap_rev smallint NOT NULL, ord int NOT NULL,
  extractor text NOT NULL, kind text NOT NULL, data jsonb NOT NULL, line int NOT NULL, enclosing_ord int NULL,
  PRIMARY KEY (org_id, blob_oid, appmap_rev, ord)
);
SELECT mage_tenant_table('app_facts');

-- per sha ---------------------------------------------------------------------------------------

CREATE TABLE manifests (
  org_id uuid NOT NULL, repo_id uuid NOT NULL,
  sha bytea NOT NULL CHECK (octet_length(sha) = 20),
  path text NOT NULL,
  blob_oid   bytea NOT NULL CHECK (octet_length(blob_oid) = 20),   -- submodule: the pinned commit
  kind       text NOT NULL CHECK (kind IN ('file','symlink','submodule')),
  executable boolean NOT NULL, lang text NULL,
  class      text NOT NULL CHECK (class IN ('source','test','generated','vendored','doc','config','data','binary')),
  size       bigint NOT NULL,
  PRIMARY KEY (org_id, repo_id, sha, path)
);
CREATE INDEX manifests_blob ON manifests (org_id, repo_id, sha, blob_oid);
SELECT mage_tenant_table('manifests');

CREATE TABLE app_nodes (
  org_id uuid NOT NULL, repo_id uuid NOT NULL,
  sha bytea NOT NULL CHECK (octet_length(sha) = 20),
  id int NOT NULL,
  kind text NOT NULL, key text NOT NULL, label text NOT NULL, framework text NULL,
  path text NULL, line int NULL,
  symbol_blob bytea NULL CHECK (octet_length(symbol_blob) = 20), symbol_ord int NULL,
  symbol_path text NULL,                                  -- SymbolRef.path: one blob may sit at several paths
  CONSTRAINT app_nodes_symbol_complete
    CHECK ((symbol_blob IS NULL) = (symbol_path IS NULL) AND (symbol_blob IS NULL) = (symbol_ord IS NULL)),
  PRIMARY KEY (org_id, repo_id, sha, id)
);
CREATE INDEX app_nodes_key ON app_nodes (org_id, repo_id, sha, kind, key);
SELECT mage_tenant_table('app_nodes');

CREATE TABLE app_edges (
  org_id uuid NOT NULL, repo_id uuid NOT NULL,
  sha bytea NOT NULL CHECK (octet_length(sha) = 20),
  src int NOT NULL, dst int NOT NULL, kind text NOT NULL, confidence real NOT NULL,
  evidence_path text NULL, evidence_line int NULL,
  PRIMARY KEY (org_id, repo_id, sha, src, dst, kind)
);
CREATE INDEX app_edges_dst ON app_edges (org_id, repo_id, sha, dst);
SELECT mage_tenant_table('app_edges');

-- per repo history ------------------------------------------------------------------------------

CREATE TABLE commits (
  org_id uuid NOT NULL, repo_id uuid NOT NULL,
  sha bytea NOT NULL CHECK (octet_length(sha) = 20),
  tree_oid bytea NOT NULL CHECK (octet_length(tree_oid) = 20),
  parents bytea[] NOT NULL, author_name text NOT NULL,
  author_time timestamptz NOT NULL, committer_time timestamptz NOT NULL,
  subject text NOT NULL, body text NOT NULL,              -- body truncated to 4 KiB (knowledge.md)
  PRIMARY KEY (org_id, repo_id, sha)
);
SELECT mage_tenant_table('commits');

CREATE TABLE commit_files (
  org_id uuid NOT NULL, repo_id uuid NOT NULL,
  sha bytea NOT NULL CHECK (octet_length(sha) = 20),
  path text NOT NULL,
  change text NOT NULL CHECK (change IN ('added','modified','deleted','renamed','type_changed')),
  old_path text NULL,
  old_blob bytea NULL CHECK (octet_length(old_blob) = 20),
  new_blob bytea NULL CHECK (octet_length(new_blob) = 20),
  adds int NULL, dels int NULL,
  PRIMARY KEY (org_id, repo_id, sha, path)
);
CREATE INDEX commit_files_path ON commit_files (org_id, repo_id, path);
SELECT mage_tenant_table('commit_files');

CREATE TABLE pull_requests (                              -- history; distinct from fix_pull_requests
  org_id uuid NOT NULL, repo_id uuid NOT NULL, number int NOT NULL,
  title text NOT NULL, body_head text NOT NULL, head_ref text NOT NULL, base_ref text NOT NULL,
  merge_commit bytea NULL CHECK (octet_length(merge_commit) = 20),
  merged_at timestamptz NULL, updated_at timestamptz NOT NULL, url text NOT NULL,
  PRIMARY KEY (org_id, repo_id, number)
);
SELECT mage_tenant_table('pull_requests');

CREATE TABLE ticket_links (
  org_id uuid NOT NULL, repo_id uuid NOT NULL, identifier text NOT NULL,
  source text NOT NULL CHECK (source IN ('commit_subject','commit_body','merge_branch','pr_title','pr_branch','pr_body')),
  commit_sha bytea NULL CHECK (octet_length(commit_sha) = 20),
  pr_number int NULL,
  UNIQUE NULLS NOT DISTINCT (org_id, repo_id, identifier, source, commit_sha, pr_number)
);
CREATE INDEX ticket_links_commit ON ticket_links (org_id, repo_id, commit_sha) WHERE commit_sha IS NOT NULL;
SELECT mage_tenant_table('ticket_links');

CREATE TABLE history_cursors (
  org_id uuid NOT NULL, repo_id uuid NOT NULL,
  walked_heads bytea[] NOT NULL, prs_updated_after timestamptz NULL, updated_at timestamptz NOT NULL,
  PRIMARY KEY (org_id, repo_id)
);
SELECT mage_tenant_table('history_cursors');

-- SCIP (per index; ingested from a Learn run's ScipIndex artifact) ------------------------------

CREATE TABLE scip_indexes (
  org_id uuid NOT NULL, scip_index_id uuid NOT NULL,
  repo_id uuid NOT NULL,
  sha bytea NOT NULL CHECK (octet_length(sha) = 20),
  artifact_id uuid NOT NULL,
  tool text NOT NULL, languages text[] NOT NULL, documents int NOT NULL,
  status text NOT NULL, created_at timestamptz NOT NULL,
  PRIMARY KEY (org_id, scip_index_id),
  UNIQUE (org_id, artifact_id),
  FOREIGN KEY (org_id, artifact_id) REFERENCES artifacts (org_id, id)
);
SELECT mage_tenant_table('scip_indexes');

CREATE TABLE scip_symbols (
  org_id uuid NOT NULL, scip_index_id uuid NOT NULL, symbol text NOT NULL,
  kind text NULL, display_name text NULL, documentation text NULL,
  relationships jsonb NOT NULL DEFAULT '[]',              -- Vec<ScipRelationship>
  PRIMARY KEY (org_id, scip_index_id, symbol)
);
SELECT mage_tenant_table('scip_symbols');

CREATE TABLE scip_occurrences (
  org_id uuid NOT NULL, scip_index_id uuid NOT NULL, path text NOT NULL, ord int NOT NULL,
  blob_oid bytea NOT NULL CHECK (octet_length(blob_oid) = 20),
  range int4[] NOT NULL, symbol text NOT NULL, roles int NOT NULL,
  enclosing_range int4[] NULL,                            -- ScipOccurrence.enclosing
  PRIMARY KEY (org_id, scip_index_id, path, ord)
);
CREATE INDEX scip_occurrences_symbol ON scip_occurrences (org_id, scip_index_id, symbol);
SELECT mage_tenant_table('scip_occurrences');
