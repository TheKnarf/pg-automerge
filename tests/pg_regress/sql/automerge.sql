-- pg_automerge: user-facing examples.
--
-- Fixture documents are Automerge saves produced by the backend (here by
-- crates/pg_automerge_core/examples/gen_regress.rs):
--   base:  a shopping list {title: Text, status, items: [{name: milk, done: false}]}
--   alice: base + milk marked done + title edited
--   bob:   base + eggs appended (concurrently with alice)
--   bob_changes: only bob's changes on top of base (save_after(base heads))
--   types: one value of every scalar type
\set VERBOSITY terse
\set base '\\x856f4a83f444e24500db0101100101010101010101010101010101010101891e9df3a1166db354fc4f7695411e61a5b6f0fd9d22eb23b814c8307f9f3573060102030213022302400256020c010402081106130b15212102230a3403420a560a57118001027f007f017f0f7f007f007f0700030c00000309017f0c020d00040800000300037e000207017f7700027d056974656d7306737461747573057469746c65000a7e04646f6e65046e616d650f007d0c7f7609017d03027f030a027d02010409017f0002017d00460009167d0001466f70656e47726f6365726965736d696c6b0f0000'
\set alice '\\x856f4a8390f29d8000a10202100101010101010101010101010101010110aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa01701dba8d4bd5c5eaeb864b31653b7e1105a5b0beb72aff635a8774474f20536c0701030303130323024003430256020e010402081108130f15222109230f3403420a560b571c8001068101028301027e00017e01007e0f0c02007e00017f00020700031800000314017f0c030d000409000a01000400037e000208017f0709017f6600037d056974656d7306737461747573057469746c6500150204646f6e657f046e616d650c000b0102007e01007d0c7f7609017f070a017c7202017e0315037d02010414017f0003017d00460014167c000102466f70656e47726f63657269657320666f722053756e6461796d696c6b18007f0102007f017f1001'
\set bob '\\x856f4a8390ac891200970202100101010101010101010101010101010110bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb010bce4d40f569ef38e7d61d12d5a9a22adfd89f7b72ad227b04798ebdbb8744bd0701030303130323024003430256020c0106020a110a130c152b2108230d3403420a560d57158001027e00017e01007e0f0302007e00017f00020700030d00020100030901020c020d02100004080000017f00000400037e000207017e770d00047d056974656d7306737461747573057469746c65000b7c04646f6e65046e616d6504646f6e65046e616d650d007f01020002017d0c7f7609010203027f7e047f030b047d0201040901020004017d004600091602007c014601466f70656e47726f6365726965736d696c6b65676773120001'
\set bob_changes '\\x856f4a830bce4d40018b0101891e9df3a1166db354fc4f7695411e61a5b6f0fd9d22eb23b814c8307f9f357310bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb011000000110010101010101010101010101010101010a0104020411041304150d340342045604570470027f0102007f0c02107f0100027f0d000200017e046e616d6504646f6e650001027f0002017d004601656767730300'
\set types '\\x856f4a8309b473d800f50101100202020202020202020202020202020201d60838f4abb5cdef94014de4e96537b2a98bd7fdff4aa9185f1a2ebc63ce8841060102030213022302400256020a15392102230c34014204560f572c8001058101028301027f007f017f0b7f007f007f07770661766174617204626f6f6c076372656174656405666c6f617403696e74036e616e046e756c6c037374720475696e7402067669736974730b00750b7b047a7e03027a0205010b0a017f057547026985011485010056a3011814deadbeefaeb683c1cc310000000000000a4056000000000000f87f68656c6c6fffffffffffffffffff01012909007e01007f007f0900'
\set incremental '\\x856f4a83f444e24500db0101100101010101010101010101010101010101891e9df3a1166db354fc4f7695411e61a5b6f0fd9d22eb23b814c8307f9f3573060102030213022302400256020c010402081106130b15212102230a3403420a560a57118001027f007f017f0f7f007f007f0700030c00000309017f0c020d00040800000300037e000207017f7700027d056974656d7306737461747573057469746c65000a7e04646f6e65046e616d650f007d0c7f7609017d03027f030a027d02010409017f0002017d00460009167d0001466f70656e47726f6365726965736d696c6b0f0000856f4a8379852149017201891e9df3a1166db354fc4f7695411e61a5b6f0fd9d22eb23b814c8307f9f357310030303030303030303030303030303030110000001100101010101010101010101010101010108150834014202560257067002710273027f06737461747573017f017f66636c6f7365647f017f017f0b'

-- Store documents. Values from the backend arrive as bytea (Automerge.save());
-- the bytea -> automerge cast validates and normalizes them.
CREATE TABLE docs (id int PRIMARY KEY, doc automerge NOT NULL);
INSERT INTO docs VALUES (1, :'base'::bytea), (2, :'types'::bytea);

-- Read them with any jsonb operator or function, no cast needed.
-- (To combine with jsonb, cast explicitly: doc || '{...}' means merge().)
SELECT doc::jsonb - 'items' || '{"source": "pg"}' AS summary FROM docs WHERE id = 1;
SELECT id, doc->>'title' AS title, doc->'items'->0->>'name' AS first_item FROM docs WHERE id = 1;
SELECT id FROM docs WHERE doc @> '{"status": "open"}';
SELECT jsonb_pretty(doc::jsonb) FROM docs WHERE id = 2;
SELECT jsonb_typeof(doc->'uint') AS uint_type, doc->>'uint' AS uint_exact FROM docs WHERE id = 2;

-- Two backends persisted concurrent edits. merge() keeps both.
UPDATE docs SET doc = merge(doc, :'alice'::automerge) WHERE id = 1;
UPDATE docs SET doc = merge(doc, :'bob'::automerge) WHERE id = 1;
SELECT doc->>'title' AS title, jsonb_path_query_array(doc, '$.items[*].name') AS items,
       jsonb_path_query_array(doc, '$.items[*] ? (@.done == false).name') AS todo
FROM docs WHERE id = 1;

-- Incremental persistence: the backend sends only the new changes
-- (save_incremental() / save_after() output, one or more concatenated
-- change chunks) as bytea, and merge(automerge, bytea) applies them on top
-- of the stored document. A driver's typed bytea parameter selects this
-- overload without a cast.
INSERT INTO docs VALUES (4, :'base'::bytea);
PREPARE persist(int, bytea) AS UPDATE docs SET doc = merge(doc, $2) WHERE id = $1;
EXECUTE persist(4, :'bob_changes');
SELECT jsonb_path_query_array(doc, '$.items[*].name') AS items FROM docs WHERE id = 4;
-- Changes already present are a no-op (the stored bytes come back as is).
SELECT merge(doc, :'bob_changes'::bytea)::bytea = doc::bytea AS noop,
       (doc || :'bob_changes'::bytea)::bytea = doc::bytea AS operator_noop,
       merge(doc, ''::bytea)::bytea = doc::bytea AS empty_noop
FROM docs WHERE id = 4;
-- Changes whose dependencies the document lacks are rejected, naming them.
SELECT merge(''::bytea::automerge, :'bob_changes'::bytea);
-- An untyped literal resolves to merge(automerge, automerge), which needs a
-- complete document: cast change chunks to bytea.
SELECT merge(doc, :'bob_changes') FROM docs WHERE id = 4;
DEALLOCATE persist;
DELETE FROM docs WHERE id = 4;

-- Upsert pattern: merge into the existing row, or insert.
INSERT INTO docs VALUES (1, :'base'::bytea)
ON CONFLICT (id) DO UPDATE SET doc = merge(docs.doc, EXCLUDED.doc);
INSERT INTO docs VALUES (3, :'bob'::bytea)
ON CONFLICT (id) DO UPDATE SET doc = merge(docs.doc, EXCLUDED.doc);
SELECT id, jsonb_array_length(doc->'items') AS n_items FROM docs WHERE id IN (1, 3) ORDER BY id;

-- merge is commutative and idempotent; || is the same operation.
SELECT merge(a, b)::jsonb = merge(b, a)::jsonb AS same_state,
       automerge_heads(merge(a, b)) = automerge_heads(merge(b, a)) AS same_heads,
       merge(merge(a, b), b)::bytea = merge(a, b)::bytea AS idempotent,
       (a || b)::bytea = merge(a, b)::bytea AS operator
FROM (SELECT :'alice'::automerge AS a, :'bob'::automerge AS b) v;

-- Merging something already contained is a no-op that returns the input bytes.
SELECT automerge_contains(doc, :'alice'::automerge) AS has_alice,
       automerge_contains(:'alice'::automerge, doc) AS alice_has_all,
       merge(doc, :'base'::automerge)::bytea = doc::bytea AS noop
FROM docs WHERE id = 1;

-- Heads: sorted hex change hashes (read from the stored bytes' header; the
-- document is not loaded).
SELECT cardinality(automerge_heads(doc)) AS n_heads,
       length((automerge_heads(doc))[1]) AS hash_len
FROM docs WHERE id = 1;
SELECT automerge_heads(''::bytea::automerge) AS empty_doc_heads, ''::bytea::automerge::jsonb AS empty_doc;

-- merge_agg folds many versions (e.g. a history table) into one document.
SELECT merge_agg(v)->>'title' AS title, jsonb_array_length(merge_agg(v)->'items') AS n_items
FROM (VALUES (:'base'::automerge), (:'alice'::automerge), (:'bob'::automerge), (NULL)) t(v);

-- Loadable input formats: compressed saves and incremental saves
-- (document + trailing changes) are accepted and normalized.
SELECT (:'incremental'::bytea::automerge)->>'status' AS status,
       length(:'incremental'::bytea) <> length(:'incremental'::bytea::automerge::bytea) AS normalized;

-- Text I/O is lossless hex, so pg_dump / COPY round-trip exactly.
SELECT (doc::text)::automerge::bytea = doc::bytea AS text_round_trip,
       left(doc::text, 10) AS text_prefix
FROM docs WHERE id = 1;

-- For read-heavy tables: a stored generated column plus a GIN index.
ALTER TABLE docs ADD COLUMN data jsonb GENERATED ALWAYS AS (doc::jsonb) STORED;
CREATE INDEX docs_data_idx ON docs USING gin (data jsonb_path_ops);
CREATE INDEX docs_expr_idx ON docs USING gin ((doc::jsonb));
SET enable_seqscan = off;
EXPLAIN (COSTS OFF) SELECT id FROM docs WHERE data @> '{"status": "open"}';
EXPLAIN (COSTS OFF) SELECT id FROM docs WHERE doc::jsonb @> '{"status": "open"}';
SELECT id FROM docs WHERE data @> '{"status": "open"}' ORDER BY id;
RESET enable_seqscan;

-- Invalid input is rejected with SQLSTATE 22P02.
SELECT '{"title": "not automerge"}'::automerge;
SELECT '\x0102'::automerge;
SELECT '\x0102'::bytea::automerge;
SELECT '\xabc'::automerge;
DO $$
BEGIN
    PERFORM '\x0102'::bytea::automerge;
EXCEPTION WHEN invalid_text_representation THEN
    RAISE WARNING 'caught SQLSTATE %', SQLSTATE;
END $$;

-- No jsonb -> automerge cast (it would fabricate history).
SELECT '{}'::jsonb::automerge;

DROP TABLE docs;
