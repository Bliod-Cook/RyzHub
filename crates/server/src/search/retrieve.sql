WITH eligible AS NOT MATERIALIZED (
 SELECT d.*,d.tableoid AS doc_tableoid,d.ctid AS doc_ctid FROM search_docs d JOIN search_sources s ON s.org_id=d.org_id AND s.id=d.id AND s.doc_kind=d.doc_kind
 WHERE d.org_id=$1 AND d.owner_user_id=$2 AND s.owner_user_id=$2 AND NOT d.deleted AND NOT s.deleted
   AND d.source_version=s.source_version
   AND EXISTS(SELECT 1 FROM memberships WHERE org_id=$1 AND user_id=$2)
   AND ($4::uuid IS NULL OR s.project_id=$4) AND ($5::uuid IS NULL OR s.conversation_id=$5)
   AND ($11::timestamptz IS NULL OR s.created_at>=$11) AND ($12::timestamptz IS NULL OR s.created_at<=$12)
   AND (($8='history' AND s.doc_kind IN ('title','message','project_file')) OR ($8='conversations' AND s.doc_kind IN ('title','message')) OR s.doc_kind=$8)
   AND ($14::uuid IS NULL OR s.doc_kind<>'project_file' OR EXISTS (
     SELECT 1 FROM runs r JOIN project_revisions rev ON rev.org_id=r.org_id AND rev.id=r.project_revision_id
     JOIN project_files file ON file.org_id=s.org_id AND file.id=s.id
     WHERE r.org_id=$1 AND r.id=$14 AND rev.files->>file.name=file.content))
   AND (NOT $9::boolean OR (($10::uuid IS NOT NULL AND s.project_id=$10) OR ($10 IS NULL AND s.memory_mode<>'project_only')))
), fulltext AS (
 SELECT id,row_number() OVER(ORDER BY pgroonga_score(doc_tableoid,doc_ctid) DESC,created_at DESC,id) AS rank FROM eligible
 WHERE $13<>'vector' AND text &@~ pgroonga_query_escape($3) ORDER BY pgroonga_score(doc_tableoid,doc_ctid) DESC,created_at DESC,id LIMIT 100
), nearest AS (
 SELECT d.id,c.embedding <=> $6::text::halfvec AS distance
 FROM search_chunks c JOIN eligible d ON d.org_id=c.org_id AND d.id=c.doc_id
 WHERE c.org_id=$1 AND $6::text IS NOT NULL AND $13<>'fulltext' AND c.embedding_model=$7 AND c.source_version=d.source_version
 ORDER BY c.embedding <=> $6::text::halfvec LIMIT 100
), semantic AS (
 SELECT id,row_number() OVER(ORDER BY min(distance),id) AS rank FROM nearest GROUP BY id
), fused AS (
 SELECT id,sum(score) AS score FROM (
  SELECT id,1.0/(60+rank) AS score FROM fulltext UNION ALL
  SELECT id,1.0/(60+rank) FROM semantic
 ) ranked GROUP BY id
)
SELECT jsonb_build_object('id',d.id,'kind',d.doc_kind,'conversation_id',d.conversation_id,
 'item_id',d.item_id,'seq',d.seq,'project_id',s.project_id,'text',left(d.text,1500),
 'file',(SELECT name FROM project_files WHERE org_id=d.org_id AND id=d.id),
 'snippet_html',array_to_string(pgroonga_snippet_html(d.text,ARRAY[$3]),' … '),'score',f.score,
 'source',CASE WHEN d.conversation_id IS NOT NULL THEN '/orgs/'||d.org_id||'/conversations/'||d.conversation_id||'?item='||coalesce(d.item_id::text,'')||'&seq='||d.seq ELSE NULL END)
FROM fused f JOIN eligible d ON d.id=f.id JOIN search_sources s ON s.org_id=d.org_id AND s.id=d.id AND s.doc_kind=d.doc_kind
ORDER BY f.score DESC,d.id LIMIT 20
