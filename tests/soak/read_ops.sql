-- jsonb operators on a document of up to a tenth of SOAK_DOC_SIZE (slots
-- 0-17; read_big.sql: the big ones), each accessor loads it, and its heads
-- (read from the stored header).
SELECT last_value AS maxid FROM soak_id \gset
\set id 20 * random(0, :maxid / 20) + random(0, 17)
SELECT doc->>'title', doc->'status', cardinality(automerge_heads(doc)) FROM soak_docs WHERE id = :id;
