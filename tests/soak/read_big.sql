-- jsonb operators on a big document (slots 18-19; read_ops.sql: the
-- others), each accessor loads it, and its heads (read from the stored
-- header).
SELECT last_value AS maxid FROM soak_id \gset
\set id 20 * random(0, :maxid / 20) + 18 + random(0, 1)
SELECT doc->>'title', doc->'status', cardinality(automerge_heads(doc)) FROM soak_docs WHERE id = :id;
