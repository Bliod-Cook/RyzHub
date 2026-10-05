-- M4. Extensions are deployment prerequisites; no silent LIKE or synthetic-vector fallback.
CREATE EXTENSION IF NOT EXISTS pgroonga;
CREATE EXTENSION IF NOT EXISTS vector;
DO $$ BEGIN
 IF (SELECT string_to_array(extversion,'.')::int[] < ARRAY[0,8,2] FROM pg_extension WHERE extname='vector') THEN
  RAISE EXCEPTION 'M4 requires pgvector >= 0.8.2';
 END IF;
END $$;
ALTER TABLE conversations ADD COLUMN search_revision bigint NOT NULL DEFAULT 1;
ALTER TABLE conversations ADD COLUMN memory_snapshot jsonb;
ALTER TABLE conversations ADD COLUMN digest_attempts integer NOT NULL DEFAULT 0;
ALTER TABLE conversations ADD COLUMN digest_available_at timestamptz NOT NULL DEFAULT now();
ALTER TABLE runs ADD COLUMN memory_announced boolean NOT NULL DEFAULT false;
ALTER TABLE items ADD COLUMN created_at timestamptz NOT NULL DEFAULT now();
UPDATE items i SET created_at=coalesce((SELECT started_at FROM runs r WHERE r.org_id=i.org_id AND r.id=i.run_id),c.created_at)
 FROM conversations c WHERE c.org_id=i.org_id AND c.id=i.conversation_id;
ALTER TABLE usage_records ALTER COLUMN run_id DROP NOT NULL;
ALTER TABLE project_files ADD COLUMN revision bigint NOT NULL DEFAULT 1;
CREATE TABLE memory_preferences (
 org_id uuid NOT NULL REFERENCES organizations, user_id uuid NOT NULL REFERENCES users,
 enabled boolean NOT NULL DEFAULT true, reference_history boolean NOT NULL DEFAULT true,
 history_disabled_at timestamptz, PRIMARY KEY(org_id,user_id)
);
CREATE TABLE memory_policy_freezes (
 org_id uuid PRIMARY KEY REFERENCES organizations, disabled_at timestamptz NOT NULL DEFAULT now(), cleaned_at timestamptz
);
CREATE TABLE memory_items (
 id uuid PRIMARY KEY, org_id uuid NOT NULL REFERENCES organizations, user_id uuid NOT NULL REFERENCES users,
 scope text NOT NULL CHECK(scope IN ('user','project')), project_id uuid,
 content text NOT NULL, priority integer NOT NULL DEFAULT 0 CHECK(priority BETWEEN -100 AND 100),
 status text NOT NULL DEFAULT 'active' CHECK(status IN ('active','archived','deleted')),
 source_conversation_id uuid, superseded_by uuid, version bigint NOT NULL DEFAULT 1,
 valid_from timestamptz NOT NULL DEFAULT now(), invalid_at timestamptz, last_used_at timestamptz,
 created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE(org_id,id), CHECK((scope='project')=(project_id IS NOT NULL)),
 FOREIGN KEY(org_id,project_id) REFERENCES projects(org_id,id),
 FOREIGN KEY(org_id,source_conversation_id) REFERENCES conversations(org_id,id)
);
CREATE TABLE memory_events (
 id uuid PRIMARY KEY, org_id uuid NOT NULL, user_id uuid NOT NULL,
 memory_id uuid NOT NULL, action text NOT NULL, before_state jsonb, after_state jsonb,
 request_key text, request_fingerprint bytea, emitted boolean NOT NULL DEFAULT false, undone_at timestamptz, created_at timestamptz NOT NULL DEFAULT now(),
 origin_conversation_id uuid,
 FOREIGN KEY(org_id,memory_id) REFERENCES memory_items(org_id,id), UNIQUE(org_id,user_id,request_key)
);
CREATE INDEX memory_events_origin ON memory_events(org_id,origin_conversation_id,created_at) WHERE NOT emitted;
CREATE TABLE conversation_digests (
 org_id uuid NOT NULL, conversation_id uuid PRIMARY KEY, user_id uuid NOT NULL,
 title text NOT NULL, user_points text NOT NULL, source_version bigint NOT NULL, source_revision bigint NOT NULL,
 updated_at timestamptz NOT NULL DEFAULT now(),
 FOREIGN KEY(org_id,conversation_id) REFERENCES conversations(org_id,id)
);
CREATE TABLE search_jobs (
 org_id uuid NOT NULL REFERENCES organizations, source_id uuid NOT NULL,
 source_kind text NOT NULL CHECK(source_kind IN ('title','message','project_file','memory')),
 source_version bigint NOT NULL, op text NOT NULL CHECK(op IN ('upsert','delete')),
 state text NOT NULL DEFAULT 'queued' CHECK(state IN ('queued','running','completed','dead')),
 attempts integer NOT NULL DEFAULT 0, available_at timestamptz NOT NULL DEFAULT now(),
 lease_until timestamptz, claim_token uuid, error text, created_at timestamptz NOT NULL DEFAULT now(),
 priority integer NOT NULL DEFAULT 0,
 PRIMARY KEY(org_id,source_id,source_version)
);
CREATE INDEX search_jobs_ready ON search_jobs(org_id,priority DESC,available_at) WHERE state IN ('queued','running');
CREATE TABLE search_docs (
 org_id uuid NOT NULL REFERENCES organizations, id uuid NOT NULL, owner_user_id uuid NOT NULL,
 project_id uuid, conversation_id uuid, item_id uuid, seq bigint NOT NULL DEFAULT 0,
 doc_kind text NOT NULL, text text NOT NULL, source_version bigint NOT NULL,
 deleted boolean NOT NULL DEFAULT false, created_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY(org_id,id)
);
CREATE INDEX search_docs_text ON search_docs USING pgroonga(text);
CREATE INDEX search_docs_owner ON search_docs(org_id,owner_user_id,created_at);
CREATE TABLE search_chunks (
 id uuid PRIMARY KEY, org_id uuid NOT NULL, doc_id uuid NOT NULL, chunk_no integer NOT NULL,
 text text NOT NULL, embedding halfvec(1024) NOT NULL, embedding_model text NOT NULL,
 dim integer NOT NULL DEFAULT 1024 CHECK(dim=1024), source_version bigint NOT NULL,
 FOREIGN KEY(org_id,doc_id) REFERENCES search_docs(org_id,id) ON DELETE CASCADE,
 UNIQUE(org_id,doc_id,chunk_no,embedding_model)
);
CREATE INDEX search_chunks_vector ON search_chunks USING hnsw(embedding halfvec_cosine_ops);
CREATE INDEX memory_owner ON memory_items(org_id,user_id,status,project_id);
CREATE INDEX memory_content ON memory_items USING pgroonga(content);
DO $$ DECLARE t text; BEGIN
 FOREACH t IN ARRAY ARRAY['memory_preferences','memory_policy_freezes','memory_items','memory_events','conversation_digests','search_jobs','search_docs','search_chunks'] LOOP
  EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY',t);
  EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY',t);
  EXECUTE format('CREATE POLICY org_isolation ON %I USING (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid) WITH CHECK (org_id=nullif(current_setting(''app.org_id'',true),'''')::uuid)',t);
  EXECUTE format('GRANT SELECT,INSERT,UPDATE,DELETE ON %I TO ryzhub_app',t);
  EXECUTE format('GRANT SELECT ON %I TO ryzhub_worker',t);
 END LOOP;
END $$;
-- All indexed results join this view in the retrieval SQL. Indexed ACLs never grant access.
CREATE VIEW search_sources WITH (security_invoker=true) AS
 SELECT c.org_id,c.id,c.creator_id AS owner_user_id,c.project_id,c.id AS conversation_id,
        NULL::uuid AS item_id,0::bigint AS seq,'title'::text AS doc_kind,c.title AS text,
        c.search_revision AS source_version,c.created_at,
        (c.temporary OR c.state NOT IN ('active','archived') OR (c.project_id IS NOT NULL AND p.state<>'active')) AS deleted,
        coalesce(p.memory_mode,'default') AS memory_mode
 FROM conversations c LEFT JOIN projects p ON p.org_id=c.org_id AND p.id=c.project_id
 UNION ALL
 SELECT i.org_id,i.id,c.creator_id,c.project_id,c.id,i.id,i.first_seq,'message',
        CASE WHEN i.kind='tool' THEN coalesce(i.detail->'kind'->'Tool'->>'name','Tool call') ELSE i.text END,
        i.revision,i.created_at,
        (c.temporary OR c.state NOT IN ('active','archived') OR i.state NOT IN ('completed','failed')
         OR i.kind NOT IN ('user','assistant','tool') OR (c.project_id IS NOT NULL AND p.state<>'active')),
        coalesce(p.memory_mode,'default')
 FROM items i JOIN conversations c ON c.org_id=i.org_id AND c.id=i.conversation_id
 LEFT JOIN projects p ON p.org_id=c.org_id AND p.id=c.project_id
 UNION ALL
 SELECT f.org_id,f.id,p.creator_id,p.id,NULL,NULL,0,'project_file',f.name || E'\n' || f.content,
        f.revision,f.created_at,p.state<>'active',p.memory_mode
 FROM project_files f JOIN projects p ON p.org_id=f.org_id AND p.id=f.project_id
 UNION ALL
 SELECT m.org_id,m.id,m.user_id,m.project_id,NULL,NULL,0,'memory',m.content,m.version,m.created_at,
        (m.status<>'active' OR (m.project_id IS NOT NULL AND p.state<>'active')),coalesce(p.memory_mode,'default')
 FROM memory_items m LEFT JOIN projects p ON p.org_id=m.org_id AND p.id=m.project_id;
GRANT SELECT ON search_sources TO ryzhub_app;

CREATE FUNCTION enqueue_search() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r record; v bigint; operation text; BEGIN
 IF TG_OP='DELETE' THEN r:=OLD; operation:='delete'; ELSE r:=NEW; operation:='upsert'; END IF;
 v:=(to_jsonb(r)->>CASE TG_ARGV[0] WHEN 'title' THEN 'search_revision' WHEN 'memory' THEN 'version' ELSE 'revision' END)::bigint;
 IF TG_OP='DELETE' THEN v:=v+1; END IF;
 INSERT INTO search_jobs(org_id,source_id,source_kind,source_version,op)
 VALUES(r.org_id,r.id,TG_ARGV[0],v,operation)
 ON CONFLICT(org_id,source_id,source_version) DO UPDATE SET op=excluded.op,state='queued',
   attempts=0,priority=0,available_at=now(),claim_token=NULL,lease_until=NULL;
 RETURN NULL;
END $$;
CREATE FUNCTION bump_search_revision() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
 IF TG_TABLE_NAME='conversations' THEN
  NEW.search_revision:=OLD.search_revision+1;
  IF NEW.project_id IS DISTINCT FROM OLD.project_id OR NEW.temporary IS DISTINCT FROM OLD.temporary THEN NEW.memory_snapshot:=NULL; END IF;
 ELSE NEW.revision:=OLD.revision+1; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER conversation_search_version BEFORE UPDATE OF title,creator_id,project_id,state,temporary ON conversations
 FOR EACH ROW EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER file_search_version BEFORE UPDATE OF content,name ON project_files FOR EACH ROW EXECUTE FUNCTION bump_search_revision();
CREATE TRIGGER conversation_search AFTER INSERT OR UPDATE OF title,creator_id,project_id,state,temporary OR DELETE ON conversations
 FOR EACH ROW EXECUTE FUNCTION enqueue_search('title');
CREATE TRIGGER item_search AFTER INSERT OR UPDATE OF text,state,revision OR DELETE ON items
 FOR EACH ROW EXECUTE FUNCTION enqueue_search('message');
CREATE TRIGGER file_search AFTER INSERT OR UPDATE OF content,name OR DELETE ON project_files
 FOR EACH ROW EXECUTE FUNCTION enqueue_search('project_file');
CREATE TRIGGER memory_search AFTER INSERT OR UPDATE OF content,status,version OR DELETE ON memory_items
 FOR EACH ROW EXECUTE FUNCTION enqueue_search('memory');
-- Existing private data is backfilled through exactly the same versioned queue.
INSERT INTO search_jobs(org_id,source_id,source_kind,source_version,op,priority)
 SELECT org_id,id,doc_kind,source_version,'upsert',-10 FROM search_sources;

-- Re-publish same-version content when ownership/boundaries change; query authorization
-- already takes effect immediately, before these jobs run.
CREATE FUNCTION enqueue_conversation_children() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
 INSERT INTO search_jobs(org_id,source_id,source_kind,source_version,op)
 SELECT org_id,id,'message',revision,'upsert' FROM items WHERE org_id=NEW.org_id AND conversation_id=NEW.id
 ON CONFLICT(org_id,source_id,source_version) DO UPDATE SET state='queued',attempts=0,priority=0,
   available_at=now(),claim_token=NULL,lease_until=NULL;
 RETURN NULL;
END $$;
CREATE TRIGGER conversation_children_search AFTER UPDATE OF creator_id,project_id,state,temporary ON conversations
 FOR EACH ROW EXECUTE FUNCTION enqueue_conversation_children();
CREATE FUNCTION enqueue_project_files() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
 INSERT INTO search_jobs(org_id,source_id,source_kind,source_version,op)
 SELECT org_id,id,'project_file',revision,'upsert' FROM project_files WHERE org_id=NEW.org_id AND project_id=NEW.id
 ON CONFLICT(org_id,source_id,source_version) DO UPDATE SET state='queued',attempts=0,priority=0,
   available_at=now(),claim_token=NULL,lease_until=NULL;
 INSERT INTO search_jobs(org_id,source_id,source_kind,source_version,op)
 SELECT org_id,id,'memory',version,'upsert' FROM memory_items WHERE org_id=NEW.org_id AND project_id=NEW.id
 ON CONFLICT(org_id,source_id,source_version) DO UPDATE SET state='queued',attempts=0,priority=0,
   available_at=now(),claim_token=NULL,lease_until=NULL;
 RETURN NULL;
END $$;
CREATE TRIGGER project_files_search AFTER UPDATE OF state ON projects FOR EACH ROW EXECUTE FUNCTION enqueue_project_files();
-- Read-only BYPASSRLS worker discovers changed digest watermarks; all mutations use tenant RLS.
GRANT SELECT ON items TO ryzhub_worker;
