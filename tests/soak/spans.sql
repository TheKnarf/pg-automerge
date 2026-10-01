-- automerge_spans on rich texts: mostly the small notes (slots 10-13), a
-- tenth the long note (slot 17), a fiftieth the big text (slot 19).
SELECT last_value AS maxid FROM soak_id \gset
\set r random(0, 49)
\set slot 10 + random(0, 3)
\if :r < 5
\set slot 17
\elif :r = 5
\set slot 19
\endif
\set id 20 * random(0, :maxid / 20) + :slot
SELECT jsonb_array_length(automerge_spans(doc, '{body}')) FROM soak_docs WHERE id = :id;
