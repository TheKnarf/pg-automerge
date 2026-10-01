-- The history functions on the small documents (slots 0-13): the change
-- list, the changes since the template's heads as bytes and as rows with
-- their bytes, the newest change, and the state as of the template.
SELECT last_value AS maxid FROM soak_id \gset
\set id 20 * random(0, :maxid / 20) + random(0, 13)
SELECT count(*), max(seq) FROM soak_docs, automerge_changes_meta(doc) WHERE id = :id;
SELECT length(automerge_changes_bytes(d.doc, b.heads)) FROM soak_docs d JOIN soak_base b USING (slot) WHERE d.id = :id;
SELECT count(*), sum(length(c.change)) FROM soak_docs d JOIN soak_base b USING (slot), automerge_changes(d.doc, b.heads) c WHERE d.id = :id;
SELECT (automerge_get_change(doc, (automerge_heads(doc))[1])).seq FROM soak_docs WHERE id = :id;
SELECT automerge_to_jsonb(d.doc, b.heads)->>'status' FROM soak_docs d JOIN soak_base b USING (slot) WHERE d.id = :id;
