-- pg_automerge: user-facing examples.
--
-- Fixture documents are Automerge saves produced by the backend (here by
-- crates/pg_automerge_core/examples/gen_regress.rs):
--   base:  a shopping list {title: Text, status, items: [{name: milk, done: false}]}
--   alice: base + milk marked done + title edited
--   bob:   base + eggs appended (concurrently with alice)
--   bob_changes: only bob's changes on top of base (save_after(base heads))
--   types: one value of every scalar type
--   log:   three commits (status draft -> review -> published) with messages and times
--   bob_reused: another writer's change on base, made with bob's actor id
\set VERBOSITY terse
\set base '\\x856f4a83f444e24500db0101100101010101010101010101010101010101891e9df3a1166db354fc4f7695411e61a5b6f0fd9d22eb23b814c8307f9f3573060102030213022302400256020c010402081106130b15212102230a3403420a560a57118001027f007f017f0f7f007f007f0700030c00000309017f0c020d00040800000300037e000207017f7700027d056974656d7306737461747573057469746c65000a7e04646f6e65046e616d650f007d0c7f7609017d03027f030a027d02010409017f0002017d00460009167d0001466f70656e47726f6365726965736d696c6b0f0000'
\set alice '\\x856f4a8390f29d8000a10202100101010101010101010101010101010110aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa01701dba8d4bd5c5eaeb864b31653b7e1105a5b0beb72aff635a8774474f20536c0701030303130323024003430256020e010402081108130f15222109230f3403420a560b571c8001068101028301027e00017e01007e0f0c02007e00017f00020700031800000314017f0c030d000409000a01000400037e000208017f0709017f6600037d056974656d7306737461747573057469746c6500150204646f6e657f046e616d650c000b0102007e01007d0c7f7609017f070a017c7202017e0315037d02010414017f0003017d00460014167c000102466f70656e47726f63657269657320666f722053756e6461796d696c6b18007f0102007f017f1001'
\set bob '\\x856f4a8390ac891200970202100101010101010101010101010101010110bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb010bce4d40f569ef38e7d61d12d5a9a22adfd89f7b72ad227b04798ebdbb8744bd0701030303130323024003430256020c0106020a110a130c152b2108230d3403420a560d57158001027e00017e01007e0f0302007e00017f00020700030d00020100030901020c020d02100004080000017f00000400037e000207017e770d00047d056974656d7306737461747573057469746c65000b7c04646f6e65046e616d6504646f6e65046e616d650d007f01020002017d0c7f7609010203027f7e047f030b047d0201040901020004017d004600091602007c014601466f70656e47726f6365726965736d696c6b65676773120001'
\set bob_changes '\\x856f4a830bce4d40018b0101891e9df3a1166db354fc4f7695411e61a5b6f0fd9d22eb23b814c8307f9f357310bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb011000000110010101010101010101010101010101010a0104020411041304150d340342045604570470027f0102007f0c02107f0100027f0d000200017e046e616d6504646f6e650001027f0002017d004601656767730300'
\set types '\\x856f4a8309b473d800f50101100202020202020202020202020202020201d60838f4abb5cdef94014de4e96537b2a98bd7fdff4aa9185f1a2ebc63ce8841060102030213022302400256020a15392102230c34014204560f572c8001058101028301027f007f017f0b7f007f007f07770661766174617204626f6f6c076372656174656405666c6f617403696e74036e616e046e756c6c037374720475696e7402067669736974730b00750b7b047a7e03027a0205010b0a017f057547026985011485010056a3011814deadbeefaeb683c1cc310000000000000a4056000000000000f87f68656c6c6fffffffffffffffffff01012909007e01007f007f0900'
\set log '\\x856f4a83119ef1ff00be0101100404040404040404040404040404040401e46f848ea5efb4303dcc871802c4c84195c7641acf6bfdf7f67218aa0fe03f6c08010203021302230a35174004430356020a15082102230234014202560557148001048101028301030300030103017fa5facdac060280a3057d06637265617465067375626d6974077075626c6973687f0002017e000103070306737461747573030003010303017d5666960164726166747265766965777075626c697368656402017f0002007e020102'
\set incremental '\\x856f4a83f444e24500db0101100101010101010101010101010101010101891e9df3a1166db354fc4f7695411e61a5b6f0fd9d22eb23b814c8307f9f3573060102030213022302400256020c010402081106130b15212102230a3403420a560a57118001027f007f017f0f7f007f007f0700030c00000309017f0c020d00040800000300037e000207017f7700027d056974656d7306737461747573057469746c65000a7e04646f6e65046e616d650f007d0c7f7609017d03027f030a027d02010409017f0002017d00460009167d0001466f70656e47726f6365726965736d696c6b0f0000856f4a8379852149017201891e9df3a1166db354fc4f7695411e61a5b6f0fd9d22eb23b814c8307f9f357310030303030303030303030303030303030110000001100101010101010101010101010101010108150834014202560257067002710273027f06737461747573017f017f66636c6f7365647f017f017f0b'
\set bob_reused '\\x856f4a832b4ce40e00950202100101010101010101010101010101010110bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb019729a1cb30068f72b74da03f18fb6ce1c8187d52569b208bbb301c779e6e28430701030303130323024003430256020e010402081106130b15232106230b3403420c560c571a8001058101028301027e00017e01007e0f0102007e00017f00020700040c00000409017f0c020d00050800000300047e000207017f7700027f056974656d7302067374617475737f057469746c65000a7e04646f6e65046e616d6502007f010d007c0c7f057109017d03027f040a027f0202017f0409017f0002017c004696010009167d0001466f70656e63616e63656c6c656447726f6365726965736d696c6b7e00010e007f017f1001'
\set bomb '\\x856f4a832d8188830167001000000000000000000000000000000001010100000009010702071107130a15083405420756057005000180ade20400000180ade204010002fface2040000017e0002feace204017f016c0080ade2040180ade2047f0280ade2040181ade2040081ade20400'

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
-- Changes whose dependencies the document lacks are rejected, naming them
-- in the DETAIL.
\set VERBOSITY default
SELECT merge(''::bytea::automerge, :'bob_changes'::bytea);
\set VERBOSITY terse
-- An untyped literal resolves to merge(automerge, automerge), which needs a
-- complete document: cast change chunks to bytea.
SELECT merge(doc, :'bob_changes') FROM docs WHERE id = 4;
-- A no-op merge still writes a new row version (and fires triggers). To
-- skip it, check first: automerge_contains(doc, bytea) says whether the
-- document already has every change in the bytes (usually without loading
-- it: new changes build on the current heads, re-sent ones are the heads).
SELECT automerge_contains(doc, :'bob_changes'::bytea) AS has_bob_changes,
       automerge_contains(:'base'::automerge, :'bob_changes'::bytea) AS base_has_them
FROM docs WHERE id = 4;
PREPARE persist_new(int, bytea) AS
    UPDATE docs SET doc = merge(doc, $2) WHERE id = $1 AND NOT automerge_contains(doc, $2)
    RETURNING id;
EXECUTE persist_new(4, :'bob_changes');  -- nothing new: no row updated
DEALLOCATE persist_new;
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

-- History (read-only): the individual changes of a document, in causal
-- order. automerge_changes_meta reads only the change graph;
-- automerge_changes also rebuilds each change's bytes (more expensive).
SET timezone = 'UTC';
SELECT seq, left(hash, 8) AS hash, left(actor, 8) AS actor, op_count, message, time,
       cardinality(deps) AS n_deps
FROM automerge_changes_meta(:'log'::automerge);
SELECT automerge_change_count(:'log'::automerge) AS n_changes,
       automerge_change_count(doc) AS n_merged
FROM docs WHERE id = 1;
-- The state as of any change (or set of heads).
SELECT message, automerge_to_jsonb(:'log'::automerge, ARRAY[hash]) AS state
FROM automerge_changes_meta(:'log'::automerge);
SELECT automerge_to_jsonb(:'log'::automerge, '{}') AS before_any_change;
-- One change by hash (NULL if the document does not have it).
SELECT c.message, c.seq, length(c.change) > 0 AS has_bytes
FROM automerge_changes_meta(:'log'::automerge) m,
     LATERAL automerge_get_change(:'log'::automerge, m.hash) c
WHERE m.seq = 2;
SELECT automerge_get_change(:'log'::automerge, repeat('0', 64)) IS NULL AS unknown_is_null;
-- Changes since a replica's heads: what alice lacks from the merged row,
-- and the same as change chunks (Automerge's save_after) the backend can
-- send to that replica or apply with merge(automerge, bytea).
SELECT left(actor, 8) AS actor, seq
FROM docs, automerge_changes_meta(doc, automerge_heads(:'alice'::automerge))
WHERE id = 1;
SELECT automerge_heads(merge(:'alice'::automerge,
                             automerge_changes_bytes(doc, automerge_heads(:'alice'::automerge))))
         = automerge_heads(doc) AS caught_up,
       automerge_changes_bytes(doc, automerge_heads(doc)) AS nothing_new
FROM docs WHERE id = 1;
-- Hashes the document does not know are ignored in since_heads (a replica
-- that is ahead); in automerge_to_jsonb they are an error (22023).
SELECT count(*) AS n FROM automerge_changes_meta(:'log'::automerge, ARRAY[repeat('ab', 32)]);
SELECT automerge_to_jsonb(:'log'::automerge, ARRAY[repeat('ab', 32)]);
SELECT automerge_changes_bytes(:'log'::automerge, ARRAY['not a hash']);
RESET timezone;

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

-- Every writer needs its own actor id. A second writer that reused bob's
-- actor id made a different first change; the two histories cannot be
-- merged (SQLSTATE 22000, data_exception, with a HINT).
\set VERBOSITY default
UPDATE docs SET doc = merge(doc, :'bob_reused'::bytea) WHERE id = 1;
\set VERBOSITY terse
SELECT set_config('regress.bob_reused', :'bob_reused', false) IS NOT NULL AS stashed;
DO $$
BEGIN
    UPDATE docs SET doc = merge(doc, current_setting('regress.bob_reused')::bytea) WHERE id = 1;
EXCEPTION WHEN data_exception THEN
    RAISE WARNING 'caught SQLSTATE %', SQLSTATE;
END $$;

-- No jsonb -> automerge cast (it would fabricate history).
SELECT '{}'::jsonb::automerge;

-- Merging many change sets in PL/pgSQL: a variable holding a merge result
-- stays loaded in memory (an expanded value), so each further
-- `d := merge(d, ...)` applies only the new changes in place; the document
-- is saved once, when it is stored. A merge that fails leaves the variable
-- as it was.
CREATE FUNCTION apply_all(p_id int, change_sets bytea[]) RETURNS int
LANGUAGE plpgsql AS $$
DECLARE
    d automerge;
    ch bytea;
    skipped int := 0;
BEGIN
    SELECT doc INTO d FROM docs WHERE id = p_id FOR UPDATE;
    FOREACH ch IN ARRAY change_sets LOOP
        BEGIN
            d := merge(d, ch);
        EXCEPTION WHEN invalid_text_representation THEN
            RAISE WARNING 'skipped: %', SQLERRM;
            skipped := skipped + 1;
        END;
    END LOOP;
    UPDATE docs SET doc = d WHERE id = p_id;
    RETURN skipped;
END $$;
INSERT INTO docs VALUES (5, :'base'::bytea);
SELECT apply_all(5, ARRAY[:'bob_changes'::bytea, '\x0102'::bytea, :'bob_changes'::bytea]) AS skipped;
SELECT jsonb_path_query_array(doc, '$.items[*].name') AS items,
       doc::bytea = merge(:'base'::automerge, :'bob_changes'::bytea)::bytea AS same_as_one_merge
FROM docs WHERE id = 5;
-- Nested merges keep intermediate results in memory too.
SELECT (:'base'::automerge || :'alice'::automerge || :'bob'::automerge)->>'title' AS title,
       automerge_heads(merge(merge(:'base'::automerge, :'alice'::automerge), :'bob_changes'::bytea))
         = automerge_heads(merge(:'alice'::automerge, :'bob'::automerge)) AS same_heads;
DROP FUNCTION apply_all(int, bytea[]);

-- Change notifications: an AFTER ROW trigger that sends
--   NOTIFY docs_changed, '{"table":"public.docs","op":"UPDATE","seq":1,"key":{"id":1},
--     "columns":{"doc":{"heads":[...],"prev_heads":[...]}}}'
-- when a row is inserted or deleted, or its automerge heads change. A
-- backend that ran LISTEN docs_changed fetches what it lacks with
-- automerge_changes_bytes(doc, <its heads>). No-op merges do not notify.
CREATE TRIGGER docs_notify AFTER INSERT OR UPDATE OR DELETE ON docs
    FOR EACH ROW EXECUTE FUNCTION automerge_notify('docs_changed', 'id');
UPDATE docs SET doc = merge(doc, :'bob'::automerge) WHERE id = 3;
-- The trigger arguments are checked when it fires (with a HINT on how to
-- declare it).
\set VERBOSITY default
CREATE TRIGGER docs_bad BEFORE UPDATE ON docs
    FOR EACH ROW EXECUTE FUNCTION automerge_notify('docs_changed', 'id');
UPDATE docs SET doc = doc WHERE id = 3;
DROP TRIGGER docs_bad ON docs;
CREATE TRIGGER docs_bad AFTER UPDATE ON docs
    FOR EACH ROW EXECUTE FUNCTION automerge_notify('docs_changed', 'uuid');
UPDATE docs SET doc = doc WHERE id = 3;
DROP TRIGGER docs_bad ON docs;
SELECT automerge_notify();
\set VERBOSITY terse

-- A few bytes can describe a document that takes gigabytes to load (here
-- a 113-byte change chunk describing 10,000,000 operations), and a failed
-- allocation would restart the server. Client input is priced from its
-- headers before anything is loaded, and rejected over
-- pg_automerge.max_load_memory (2GB by default; superuser-only) with
-- SQLSTATE 53400; so are merges whose result would exceed it. Stored
-- values are never checked when they are read, so lowering the limit
-- leaves them readable.
\set VERBOSITY default
SELECT length(:'bomb'::bytea) AS bytes;
SELECT :'bomb'::bytea::automerge;
SET pg_automerge.max_load_memory = '1MB';
SELECT doc->>'status' AS status FROM docs WHERE id = 3;
SELECT :'bob'::automerge IS NOT NULL AS small_input_fits;
RESET pg_automerge.max_load_memory;
\set VERBOSITY terse

DROP TABLE docs;
