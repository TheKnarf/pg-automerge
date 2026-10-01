-- Upsert of a full save (Automerge.save() of the backend's replica): the
-- lane's save at its next checkpoint, INSERT .. ON CONFLICT DO UPDATE SET
-- doc = merge(docs.doc, EXCLUDED.doc).
SELECT last_value AS maxid FROM soak_id \gset
\set id random(0, :maxid)
\set lane random(0, 2)
BEGIN;
WITH c AS (UPDATE soak_cursor SET pos = least(:lane_changes, (pos / :checkpoint + 1) * :checkpoint), writes = writes + 1 WHERE id = :id AND lane = :lane AND pos < :lane_changes RETURNING slot, pos) SELECT coalesce((SELECT slot FROM c), -1) AS slot, coalesce((SELECT pos FROM c), 0) AS pos \gset
\if :pos > 0
INSERT INTO soak_docs AS d (id, slot, doc) SELECT :id::bigint, :slot::int, save FROM soak_save WHERE slot = :slot AND lane = :lane AND pos = :pos ON CONFLICT (id) DO UPDATE SET doc = merge(d.doc, EXCLUDED.doc), updated_at = now();
\endif
END;
