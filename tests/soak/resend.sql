-- A re-sent change (a retry after a lost acknowledgement), skipped with
-- the WHERE NOT automerge_contains pattern of
-- docs/src/pages/guide/keeping-backends-in-sync.mdx: updates no row and
-- notifies nobody.
SELECT last_value AS maxid FROM soak_id \gset
\set id random(0, :maxid)
\set lane random(0, 2)
SELECT coalesce((SELECT slot FROM soak_cursor WHERE id = :id AND lane = :lane), -1) AS slot, coalesce((SELECT pos FROM soak_cursor WHERE id = :id AND lane = :lane), 0) AS pos \gset
\if :pos > 0
UPDATE soak_docs SET doc = merge(doc, ch.changes), updated_at = now() FROM soak_change ch WHERE id = :id AND ch.slot = :slot AND ch.lane = :lane AND ch.pos = :pos AND NOT automerge_contains(doc, ch.changes);
\endif
