-- Incremental write (the backend persisting one edit) to a document of up
-- to a tenth of SOAK_DOC_SIZE (slots 0-17; write_big.sql: the big ones):
-- the next change of a random writer lane to a random row, as UPDATE ..
-- SET doc = merge(doc, changes bytea). The lane's cursor row serializes
-- one lane; the lanes of a row are concurrent writers on it. A lane at its
-- end skips the write.
SELECT last_value AS maxid FROM soak_id \gset
\set id 20 * random(0, :maxid / 20) + random(0, 17)
\set lane random(0, 2)
BEGIN;
WITH c AS (UPDATE soak_cursor SET pos = pos + 1, writes = writes + 1 WHERE id = :id AND lane = :lane AND pos < :lane_changes RETURNING slot, pos) SELECT coalesce((SELECT slot FROM c), -1) AS slot, coalesce((SELECT pos FROM c), 0) AS pos \gset
\if :pos > 0
UPDATE soak_docs SET doc = merge(doc, (SELECT changes FROM soak_change WHERE slot = :slot AND lane = :lane AND pos = :pos)), updated_at = now() WHERE id = :id;
\endif
END;
