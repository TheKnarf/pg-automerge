-- The backend loading a document into its replica: the stored bytes.
SELECT last_value AS maxid FROM soak_id \gset
\set id random(0, :maxid)
SELECT doc::bytea FROM soak_docs WHERE id = :id;
