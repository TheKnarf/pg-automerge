-- The generated jsonb column: a containment query through its GIN index,
-- and a read of one row's copy.
SELECT last_value AS maxid FROM soak_id \gset
\set id random(0, :maxid)
\set slot random(0, 19)
\set s random(1, 3)
SELECT count(*) FROM (SELECT id FROM soak_docs WHERE data @> jsonb_build_object('slot', :slot::int, 'status', (ARRAY['open', 'review', 'closed'])[:s::int]) LIMIT 50) x;
SELECT data->'title', jsonb_array_length(coalesce(data->'items', '[]')) FROM soak_docs WHERE id = :id;
