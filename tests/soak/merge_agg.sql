-- merge_agg over four copies of one small template (rows id, id + 20, ...
-- share their base and lane actors, so they merge).
SELECT last_value AS maxid FROM soak_id \gset
\set id 20 * random(0, greatest(:maxid / 20 - 3, 0)) + random(0, 13)
SELECT m::jsonb->>'status', cardinality(automerge_heads(m)) FROM (SELECT merge_agg(doc) AS m FROM soak_docs WHERE id = ANY (ARRAY[:id::bigint, :id::bigint + 20, :id::bigint + 40, :id::bigint + 60])) x;
