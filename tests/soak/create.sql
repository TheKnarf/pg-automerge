-- A new document (the growing set of rows): the template of slot id % 20,
-- upserted as a full save, and its three lane cursors.
SELECT nextval('soak_id') AS id \gset
\set slot :id % 20
BEGIN;
INSERT INTO soak_cursor (id, lane, slot) SELECT :id::bigint, l, :slot::int FROM generate_series(0, 2) l;
INSERT INTO soak_docs AS d (id, slot, doc) SELECT :id::bigint, :slot::int, base FROM soak_template WHERE slot = :slot ON CONFLICT (id) DO UPDATE SET doc = merge(d.doc, EXCLUDED.doc), updated_at = now();
END;
