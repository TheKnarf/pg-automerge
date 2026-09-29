// Text and binary I/O, casts, jsonb access and invalid or odd input (see
// also crates/pg_automerge_core/tests/edge_cases.rs).

#[pg_test]
fn text_io_round_trip() {
    let bytes = sample().save();
    let normalized = sample().document().save_nocompress();
    let text: String = one("SELECT $1::automerge::text", &[bytes.clone().into()]);
    assert!(text.starts_with("\\x"));
    assert_eq!(
        text,
        pg_automerge_core::encoding::to_hex_literal(&normalized)
    );
    // Text output reads back to the same value.
    let again: String = one("SELECT $1::text::automerge::text", &[text.clone().into()]);
    assert_eq!(again, text);
    // Uppercase hex is accepted too.
    let upper = format!("\\x{}", text[2..].to_uppercase());
    let again: String = one("SELECT $1::text::automerge::text", &[upper.into()]);
    assert_eq!(again, text);
}

#[pg_test]
fn binary_send_recv_round_trip() {
    let bytes = sample().save();
    let normalized = sample().document().save_nocompress();
    let sent: Vec<u8> = one(
        "SELECT automerge_send($1::automerge)",
        &[bytes.clone().into()],
    );
    assert_eq!(sent, normalized);
    // recv is exercised through a binary COPY round trip.
    Spi::run("CREATE TEMP TABLE recv_src (d automerge)").unwrap();
    Spi::run_with_args("INSERT INTO recv_src VALUES ($1)", &[bytes.into()]).unwrap();
    Spi::run(
        "CREATE TEMP TABLE recv_dst AS SELECT d FROM recv_src WITH NO DATA; \
         COPY recv_src TO '/tmp/pg_automerge_recv_test.bin' WITH (FORMAT binary); \
         COPY recv_dst FROM '/tmp/pg_automerge_recv_test.bin' WITH (FORMAT binary);",
    )
    .unwrap();
    let back: Vec<u8> = one("SELECT d::bytea FROM recv_dst", &[]);
    assert_eq!(back, normalized);
}

#[pg_test]
fn casts() {
    let bytes = sample().save();
    let normalized = sample().document().save_nocompress();
    // bytea -> automerge normalizes; automerge -> bytea returns stored bytes.
    let stored: Vec<u8> = one("SELECT $1::automerge::bytea", &[bytes.clone().into()]);
    assert_eq!(stored, normalized);
    // The bytea cast is assignment-level: a bytea parameter can be
    // inserted directly.
    Spi::run("CREATE TEMP TABLE cast_docs (doc automerge)").unwrap();
    Spi::run_with_args("INSERT INTO cast_docs VALUES ($1)", &[bytes.clone().into()]).unwrap();
    // ... but not implicit (no silent bytea -> automerge in expressions).
    let implicit = Spi::get_one_with_args::<bool>(
        "SELECT castcontext = 'a' FROM pg_cast \
         WHERE castsource = 'bytea'::regtype AND casttarget = 'automerge'::regtype",
        &[],
    );
    assert_eq!(implicit.unwrap(), Some(true));
    let explicit_only = one::<bool>(
        "SELECT castcontext = 'e' FROM pg_cast \
         WHERE castsource = 'automerge'::regtype AND casttarget = 'bytea'::regtype",
        &[],
    );
    assert!(explicit_only);
    // automerge -> jsonb is implicit and its function immutable.
    let (context, volatility) = Spi::get_two::<String, String>(
        "SELECT c.castcontext::text, p.provolatile::text FROM pg_cast c \
         JOIN pg_proc p ON p.oid = c.castfunc \
         WHERE castsource = 'automerge'::regtype AND casttarget = 'jsonb'::regtype",
    )
    .unwrap();
    assert_eq!(context.as_deref(), Some("i"));
    assert_eq!(volatility.as_deref(), Some("i"));

    let json: JsonB = one("SELECT doc::jsonb FROM cast_docs", &[]);
    assert_eq!(
        json.0,
        json!({
            "status": "open",
            "title": "Groceries",
            "items": [{ "name": "milk", "done": true }, { "name": "eggs", "done": false }]
        })
    );
    // Implicit: assignable to a jsonb variable/column without a cast.
    Spi::run("CREATE TEMP TABLE cast_json (j jsonb)").unwrap();
    Spi::run("INSERT INTO cast_json SELECT doc FROM cast_docs").unwrap();
    let count: i64 = one(
        "SELECT count(*) FROM cast_json WHERE j->>'status' = 'open'",
        &[],
    );
    assert_eq!(count, 1);
}

#[pg_test]
fn jsonb_operators_on_automerge_column() {
    Spi::run("CREATE TEMP TABLE docs (id int, doc automerge)").unwrap();
    Spi::run_with_args("INSERT INTO docs VALUES (1, $1)", &[sample().save().into()]).unwrap();
    let mut other = AutoCommit::new();
    other.put(ROOT, "status", "closed").unwrap();
    Spi::run_with_args("INSERT INTO docs VALUES (2, $1)", &[other.save().into()]).unwrap();

    let title: String = one("SELECT doc->>'title' FROM docs WHERE id = 1", &[]);
    assert_eq!(title, "Groceries");
    let first: JsonB = one("SELECT doc->'items'->0 FROM docs WHERE id = 1", &[]);
    assert_eq!(first.0, json!({ "name": "milk", "done": true }));
    let id: i32 = one(
        "SELECT id FROM docs WHERE doc @> '{\"status\": \"closed\"}'",
        &[],
    );
    assert_eq!(id, 2);
    let exists: bool = one("SELECT doc ? 'items' FROM docs WHERE id = 1", &[]);
    assert!(exists);
    let open: String = one(
        "SELECT jsonb_path_query(doc, '$.items[*] ? (@.done == false).name')::text \
         FROM docs WHERE id = 1",
        &[],
    );
    assert_eq!(open, "\"eggs\"");
    let path: String = one(
        "SELECT doc #>> '{items,1,name}' FROM docs WHERE id = 1",
        &[],
    );
    assert_eq!(path, "eggs");
}

#[pg_test]
fn invalid_input_rejected_with_clear_errors() {
    for (sql, expected) in [
        (
            "SELECT '{\"a\": 1}'::automerge",
            "22P02: invalid input syntax for type automerge: expected \"\\x\" followed by hex digits",
        ),
        (
            "SELECT '\\x0'::automerge",
            "22P02: invalid input syntax for type automerge: odd number of hex digits",
        ),
        (
            "SELECT '\\xzz'::automerge",
            "22P02: invalid input syntax for type automerge: invalid hex digit 'z'",
        ),
    ] {
        assert_eq!(sql_error(sql), expected, "{sql}");
    }
    for sql in [
        "SELECT '\\x0102030405'::automerge",
        "SELECT '\\x0102030405'::bytea::automerge",
        // Automerge magic bytes, then nothing.
        "SELECT '\\x856f4a83'::bytea::automerge",
    ] {
        let err = sql_error(sql);
        assert!(
            err.starts_with("22P02: invalid automerge document: "),
            "{sql}: {err}"
        );
    }
}

#[pg_test]
fn orphaned_changes_rejected() {
    let mut doc = AutoCommit::new();
    doc.put(ROOT, "x", 1i64).unwrap();
    let heads = doc.get_heads();
    doc.put(ROOT, "y", 2i64).unwrap();
    let orphan = pg_automerge_core::encoding::to_hex_literal(&doc.save_after(&heads));
    assert_eq!(
        sql_error(&format!("SELECT '{orphan}'::bytea::automerge")),
        "22P02: invalid automerge document: changes are missing dependencies"
    );
}

#[pg_test]
fn expression_index_and_generated_column() {
    Spi::run(
        "CREATE TEMP TABLE idx_docs (id int PRIMARY KEY, doc automerge NOT NULL, \
         data jsonb GENERATED ALWAYS AS (doc::jsonb) STORED)",
    )
    .unwrap();
    Spi::run("CREATE INDEX idx_docs_expr ON idx_docs USING gin ((doc::jsonb))").unwrap();
    Spi::run("CREATE INDEX idx_docs_data ON idx_docs USING gin (data jsonb_path_ops)").unwrap();
    for i in 0..200i32 {
        let mut doc = AutoCommit::new();
        doc.put(ROOT, "status", if i == 42 { "rare" } else { "common" })
            .unwrap();
        doc.put(ROOT, "i", i as i64).unwrap();
        Spi::run_with_args(
            "INSERT INTO idx_docs (id, doc) VALUES ($1, $2)",
            &[i.into(), doc.save().into()],
        )
        .unwrap();
    }
    Spi::run("ANALYZE idx_docs").unwrap();
    Spi::run("SET LOCAL enable_seqscan = off").unwrap();

    let generated: i64 = one(
        "SELECT (data->>'i')::bigint FROM idx_docs WHERE id = 7",
        &[],
    );
    assert_eq!(generated, 7);

    let plan = explain("SELECT id FROM idx_docs WHERE doc::jsonb @> '{\"status\": \"rare\"}'");
    assert!(plan.contains("idx_docs_expr"), "{plan}");
    let id: i32 = one(
        "SELECT id FROM idx_docs WHERE doc::jsonb @> '{\"status\": \"rare\"}'",
        &[],
    );
    assert_eq!(id, 42);

    let plan = explain("SELECT id FROM idx_docs WHERE data @> '{\"status\": \"rare\"}'");
    assert!(plan.contains("idx_docs_data"), "{plan}");

    // Merging into a row keeps the generated column in sync.
    let mut extra = AutoCommit::new();
    extra.put(ROOT, "extra", true).unwrap();
    Spi::run_with_args(
        "UPDATE idx_docs SET doc = doc || $1::automerge WHERE id = 42",
        &[extra.save().into()],
    )
    .unwrap();
    let extra_flag: bool = one(
        "SELECT (data->>'extra')::bool FROM idx_docs WHERE id = 42",
        &[],
    );
    assert!(extra_flag);
}

#[pg_test]
fn empty_and_delete_only_documents() {
    let empty_bytes = pg_automerge_core::normalize(&[]).unwrap();
    for sql in [
        "SELECT ''::bytea::automerge::bytea",
        "SELECT '\\x'::automerge::bytea",
        "SELECT $1::automerge::bytea",
    ] {
        let bytes: Vec<u8> = one(sql, &[AutoCommit::new().save().into()]);
        assert_eq!(bytes, empty_bytes, "{sql}");
    }
    let json: JsonB = one("SELECT ''::bytea::automerge::jsonb", &[]);
    assert_eq!(json.0, json!({}));

    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "gone", 1i64).unwrap();
    doc.delete(ROOT, "gone").unwrap();
    let args = [doc.save().into()];
    let json: JsonB = one("SELECT $1::automerge::jsonb", &args);
    assert_eq!(json.0, json!({}));
    let n: i32 = one("SELECT cardinality(automerge_heads($1::automerge))", &args);
    assert_eq!(n, 1);
    // Merging with the empty document is a no-op in both directions.
    let same: bool = one(
        "SELECT merge($1::automerge, ''::bytea::automerge)::bytea = $1::automerge::bytea \
           AND merge(''::bytea::automerge, $1::automerge)::bytea = $1::automerge::bytea",
        &args,
    );
    assert!(same);
}

#[pg_test]
fn null_handling() {
    let mut doc = AutoCommit::new();
    doc.put(ROOT, "x", 1i64).unwrap();
    let args = [doc.save().into()];
    for expr in [
        "merge(NULL::automerge, $1::automerge)",
        "merge($1::automerge, NULL::automerge)",
        "$1::automerge || NULL::automerge",
        "automerge_heads(NULL::automerge)",
        "automerge_contains($1::automerge, NULL)",
        "automerge_contains(NULL, $1::automerge)",
        "NULL::bytea::automerge",
        "NULL::automerge::jsonb",
        "NULL::automerge::bytea",
        "(NULL::automerge)->'x'",
        "(SELECT merge_agg(d) FROM (VALUES (NULL::automerge), (NULL)) v(d))",
        "(SELECT merge_agg(d) FROM (SELECT $1::automerge WHERE false) v(d))",
    ] {
        let is_null: bool = one(&format!("SELECT ({expr}) IS NULL"), &args);
        assert!(is_null, "{expr} should be NULL");
    }
    // NULL inputs are skipped, not poisoning the aggregate.
    let json: JsonB = one(
        "SELECT merge_agg(d)::jsonb FROM (VALUES (NULL::automerge), ($1::automerge), (NULL)) v(d)",
        &args,
    );
    assert_eq!(json.0, json!({ "x": 1 }));
}

#[pg_test]
fn large_document_is_toasted_and_round_trips() {
    let mut doc = large_doc();
    let normalized = stored(&mut doc);
    assert!(normalized.len() > 2_000_000, "{}", normalized.len());
    Spi::run("CREATE TEMP TABLE big (id int PRIMARY KEY, doc automerge NOT NULL)").unwrap();
    // Compressed save in, normalized bytes stored.
    Spi::run_with_args("INSERT INTO big VALUES (1, $1)", &[doc.save().into()]).unwrap();

    // Stored out of line in the TOAST table.
    let toast_bytes: i64 = one(
        "SELECT pg_relation_size(reltoastrelid) FROM pg_class WHERE oid = 'big'::regclass",
        &[],
    );
    assert!(toast_bytes > 1_000_000, "toast size {toast_bytes}");
    let back: Vec<u8> = one("SELECT doc::bytea FROM big", &[]);
    assert_eq!(back, normalized);
    let text_round_trip: bool = one(
        "SELECT doc::text::automerge::bytea = doc::bytea FROM big",
        &[],
    );
    assert!(text_round_trip);

    let mut fork = doc.fork().with_actor(actor(2));
    doc.put(ROOT, "a", true).unwrap();
    fork.put(ROOT, "b", true).unwrap();
    Spi::run_with_args(
        "UPDATE big SET doc = merge(doc, $1::automerge)",
        &[doc.save().into()],
    )
    .unwrap();
    Spi::run_with_args(
        "UPDATE big SET doc = doc || $1::automerge",
        &[fork.save().into()],
    )
    .unwrap();
    let (a, b) =
        Spi::get_two::<bool, bool>("SELECT (doc->>'a')::bool, (doc->>'b')::bool FROM big")
            .unwrap();
    assert_eq!((a, b), (Some(true), Some(true)));
    let len: i32 = one("SELECT length(doc->>'text') FROM big", &[]);
    assert_eq!(len, 3_000_000);
    doc.merge(&mut fork).unwrap();
    let expected: Vec<String> = {
        let mut h: Vec<String> = doc.get_heads().iter().map(ToString::to_string).collect();
        h.sort();
        h
    };
    let heads: Vec<String> = one("SELECT automerge_heads(doc) FROM big", &[]);
    assert_eq!(heads, expected);
}

#[pg_test]
fn scalar_edge_cases_through_jsonb() {
    use pg_automerge_core::automerge::ScalarValue;
    let mut doc = AutoCommit::new();
    doc.put(ROOT, "imin", i64::MIN).unwrap();
    doc.put(ROOT, "imax", i64::MAX).unwrap();
    doc.put(ROOT, "umax", u64::MAX).unwrap();
    doc.put(ROOT, "neg_zero", -0.0f64).unwrap();
    doc.put(ROOT, "nan", f64::NAN).unwrap();
    doc.put(ROOT, "inf", f64::INFINITY).unwrap();
    doc.put(ROOT, "fmax", f64::MAX).unwrap();
    doc.put(ROOT, "subnormal", 5e-324f64).unwrap();
    doc.put(ROOT, "tenth", 0.1f64).unwrap();
    doc.put(ROOT, "counter", ScalarValue::counter(i64::MAX))
        .unwrap();
    doc.increment(ROOT, "counter", 1).unwrap();
    doc.put(ROOT, "before_epoch", ScalarValue::Timestamp(-1))
        .unwrap();
    doc.put(
        ROOT,
        "year_minus_1",
        ScalarValue::Timestamp(-62_167_219_200_001),
    )
    .unwrap();
    doc.put(ROOT, "bytes", ScalarValue::Bytes(vec![0, 1, 0xfe, 0xff]))
        .unwrap();
    doc.put(ROOT, "😀", "emoji key").unwrap();
    doc.put(ROOT, "nul\0key", "nul\0value").unwrap();
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    doc.splice_text(&text, 0, 0, "a👩‍👩‍👧‍👦b").unwrap();
    let args = [doc.save().into()];
    for (expr, expected) in [
        ("d->'imin' = '-9223372036854775808'", true),
        ("d->'imax' = '9223372036854775807'", true),
        ("d->'umax' = '18446744073709551615'", true),
        ("(d->>'umax')::numeric = 18446744073709551615", true),
        ("d->'neg_zero' = '0'", true),
        ("jsonb_typeof(d->'nan') = 'null'", true),
        ("jsonb_typeof(d->'inf') = 'null'", true),
        ("(d->>'fmax')::float8 = 1.7976931348623157e308", true),
        ("(d->>'subnormal')::float8 = 5e-324", true),
        ("d->>'tenth' = '0.1'", true),
        // Wraps like the release build of Automerge does.
        ("d->'counter' = '-9223372036854775808'", true),
        ("d->>'before_epoch' = '1969-12-31T23:59:59.999Z'", true),
        (
            "(d->>'before_epoch')::timestamptz = '1969-12-31 23:59:59.999+00'",
            true,
        ),
        ("d->>'year_minus_1' = '-000001-12-31T23:59:59.999Z'", true),
        ("decode(d->>'bytes', 'base64') = '\\x0001feff'::bytea", true),
        ("d->>'😀' = 'emoji key'", true),
        ("d->>'nul\u{FFFD}key' = 'nul\u{FFFD}value'", true),
        ("d->>'text' = 'a👩‍👩‍👧‍👦b'", true),
        ("d ? 'nul'", false),
    ] {
        let got: bool = one(
            &format!("SELECT {expr} FROM (SELECT $1::automerge::jsonb) v(d)"),
            &args,
        );
        assert_eq!(got, expected, "{expr}");
    }
}

#[pg_test]
fn deep_nesting_limit() {
    let nest = |depth: usize| {
        let mut doc = AutoCommit::new();
        let mut obj = ROOT;
        for _ in 0..depth {
            obj = doc.put_object(&obj, "k", ObjType::Map).unwrap();
        }
        doc.save()
    };
    let max = pg_automerge_core::json::MAX_DEPTH;
    // The deepest document still accepted converts and is queryable.
    let depth: i32 = one(
        "WITH RECURSIVE r(j, n) AS (SELECT $1::automerge::jsonb, 0 \
           UNION ALL SELECT j->'k', n + 1 FROM r WHERE j ? 'k') SELECT max(n) FROM r",
        &[nest(max - 1).into()],
    );
    assert_eq!(depth as usize, max - 1);
    let hex = pg_automerge_core::encoding::to_hex_literal(&nest(max));
    let err = sql_error(&format!("SELECT '{hex}'::automerge::jsonb"));
    assert_eq!(
        err,
        format!("54000: automerge document is nested more than {max} levels deep")
    );
    // Storing and merging it is still fine; only the jsonb view fails.
    let ok: bool = one(
        "SELECT cardinality(automerge_heads(merge($1::automerge, ''::bytea::automerge))) = 1",
        &[nest(max).into()],
    );
    assert!(ok);
}

#[pg_test]
fn incremental_and_compressed_input_is_normalized() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "v", 0i64).unwrap();
    let mut bytes = doc.save();
    for i in 1..=3i64 {
        doc.put(ROOT, "v", i).unwrap();
        doc.put(ROOT, format!("pad{i}"), "x".repeat(400)).unwrap();
        bytes.extend(doc.save_incremental());
    }
    let compressed = doc.save();
    let normalized = stored(&mut doc);
    assert!(compressed.len() < normalized.len());
    for input in [bytes, compressed, normalized.clone()] {
        let got: Vec<u8> = one("SELECT $1::automerge::bytea", &[input.into()]);
        assert_eq!(got, normalized);
    }
    // An incremental chunk on its own lacks its base and is rejected.
    let heads = doc.get_heads();
    doc.put(ROOT, "v", 4i64).unwrap();
    let hex = pg_automerge_core::encoding::to_hex_literal(&doc.save_after(&heads));
    let err = sql_error(&format!("SELECT '{hex}'::bytea::automerge"));
    assert!(err.starts_with("22P02: "), "{err}");
}

#[pg_test]
fn text_output_round_trips_exactly() {
    // What pg_dump / COPY rely on: text out -> text in is the identity.
    let mut a = AutoCommit::new().with_actor(actor(7));
    a.put(ROOT, "k", "😀").unwrap();
    let mut b = a.fork().with_actor(actor(8));
    a.put(ROOT, "k", "a").unwrap();
    b.put(ROOT, "k", "b").unwrap();
    Spi::run("CREATE TEMP TABLE dump_src (id int, doc automerge)").unwrap();
    for (i, bytes) in [a.save(), b.save(), Vec::new(), large_doc().save()]
        .into_iter()
        .enumerate()
    {
        Spi::run_with_args(
            "INSERT INTO dump_src VALUES ($1, $2)",
            &[(i as i32).into(), bytes.into()],
        )
        .unwrap();
    }
    Spi::run("INSERT INTO dump_src SELECT 10, merge_agg(doc) FROM dump_src").unwrap();
    let all_same: bool = one(
        "SELECT bool_and(doc::text::automerge::bytea = doc::bytea \
            AND automerge_heads(doc::text::automerge) = automerge_heads(doc)) FROM dump_src",
        &[],
    );
    assert!(all_same);
    // Through COPY's text format, as pg_dump does.
    Spi::run(
        "CREATE TEMP TABLE dump_dst (LIKE dump_src); \
         COPY dump_src TO '/tmp/pg_automerge_copy_test.txt'; \
         COPY dump_dst FROM '/tmp/pg_automerge_copy_test.txt';",
    )
    .unwrap();
    let mismatches: i64 = one(
        "SELECT count(*) FROM dump_src s FULL JOIN dump_dst d USING (id) \
         WHERE s.doc::bytea IS DISTINCT FROM d.doc::bytea",
        &[],
    );
    assert_eq!(mismatches, 0);
}

#[pg_test]
fn garbage_input_is_a_clean_error() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "s", "hello").unwrap();
    let l = doc.put_object(ROOT, "l", ObjType::List).unwrap();
    doc.insert(&l, 0, 1i64).unwrap();
    let save = doc.save();
    // Every truncation and a bit flip at every position: either a valid
    // document or 22P02, never another error or a crashed backend.
    let mut inputs: Vec<Vec<u8>> = (1..save.len()).map(|n| save[..n].to_vec()).collect();
    inputs.extend((0..save.len()).map(|i| {
        let mut b = save.clone();
        b[i] ^= 0x55;
        b
    }));
    inputs.push([save.as_slice(), b"trailing"].concat());
    inputs.push(vec![0; 64]);
    inputs.push(vec![0xff; 64]);
    for input in inputs {
        let hex = pg_automerge_core::encoding::to_hex_literal(&input);
        let result = sql_error(&format!("SELECT '{hex}'::bytea::automerge::jsonb"));
        assert!(
            result == "no error" || result.starts_with("22P02: invalid automerge document"),
            "{hex}: {result}"
        );
    }
}

#[pg_test]
fn comparison_operators_compare_jsonb_not_history() {
    // Documented in README/DESIGN: there is no automerge equality, so
    // `=` resolves through the implicit cast to jsonb equality of the
    // current state. Same content, different histories: equal as jsonb,
    // different heads.
    let mut a = AutoCommit::new().with_actor(actor(1));
    let mut b = AutoCommit::new().with_actor(actor(2));
    a.put(ROOT, "x", 1i64).unwrap();
    b.put(ROOT, "x", 1i64).unwrap();
    let args = [a.save().into(), b.save().into()];
    assert!(one::<bool>("SELECT $1::automerge = $2::automerge", &args));
    assert!(!one::<bool>(
        "SELECT automerge_heads($1::automerge) = automerge_heads($2::automerge)",
        &args
    ));
    assert_eq!(
        sql_error("SELECT DISTINCT '\\x'::automerge"),
        "42883: could not identify an equality operator for type automerge"
    );
    // merge is commutative in heads and jsonb.
    let mut c = a.fork().with_actor(actor(3));
    c.put(ROOT, "y", 2i64).unwrap();
    a.put(ROOT, "z", 3i64).unwrap();
    let args = [a.save().into(), c.save().into()];
    assert!(one::<bool>(
        "SELECT automerge_heads(merge($1::automerge, $2::automerge)) = automerge_heads(merge($2::automerge, $1::automerge)) \
           AND merge($1::automerge, $2::automerge)::jsonb = merge($2::automerge, $1::automerge)::jsonb",
        &args
    ));
}

#[pg_test]
fn checksummed_garbage_is_a_clean_error() {
    // Mutations of a chunk body with the checksum recomputed get past
    // Automerge's integrity check into its column decoders, which panic
    // on some malformed data. That must still be 22P02, not XX000.
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "s", "hello").unwrap();
    doc.put(ROOT, "n", -5i64).unwrap();
    let l = doc.put_object(ROOT, "l", ObjType::List).unwrap();
    doc.insert(&l, 0, 1.5f64).unwrap();
    let t = doc.put_object(ROOT, "t", ObjType::Text).unwrap();
    doc.splice_text(&t, 0, 0, "text").unwrap();
    let save = doc.save_nocompress();
    // One uncompressed document chunk: magic (4), checksum (4), type (1),
    // uleb128 length, data.
    let (typ, data) = {
        let (mut len, mut n) = (0usize, 0);
        while save[9 + n] & 0x80 != 0 {
            len |= ((save[9 + n] & 0x7f) as usize) << (7 * n);
            n += 1;
        }
        len |= (save[9 + n] as usize) << (7 * n);
        (save[8], save[10 + n..10 + n + len].to_vec())
    };
    // The chunk with a valid checksum for `data`, computed in SQL:
    // sha256(type || uleb len || data)[..4].
    let check = "SELECT pg_temp.sql_error(format('SELECT %L::bytea::automerge::jsonb', \
                 '\\x856f4a83'::bytea || substr(sha256($1), 1, 4) || $1))";
    sql_error("SELECT 1"); // creates pg_temp.sql_error
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut rand = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut decoder_panics = 0;
    for _ in 0..1500 {
        let mut data = data.clone();
        for _ in 0..1 + rand() % 2 {
            let i = (rand() % data.len() as u64) as usize;
            data[i] = rand() as u8;
        }
        let mut chunk = vec![typ];
        let mut len = data.len();
        while len >= 0x80 {
            chunk.push((len as u8 & 0x7f) | 0x80);
            len >>= 7;
        }
        chunk.push(len as u8);
        chunk.extend(&data);
        let result: String = one(check, &[chunk.into()]);
        assert!(
            result == "no error" || result.starts_with("22P02: invalid automerge document"),
            "{result}"
        );
        if result.contains("malformed data") {
            decoder_panics += 1;
        }
    }
    assert!(decoder_panics > 0, "no decoder panic provoked");
}

#[pg_test]
fn every_extension_object_has_a_comment() {
    // Members of the extension (pg_depend deptype 'e') among functions and
    // aggregates, types, operators and casts that have no COMMENT, except
    // the pg_tests themselves (schema tests) and the implicit array types.
    let uncommented: Option<String> = Spi::get_one(
        "SELECT string_agg(pg_describe_object(d.classid, d.objid, d.objsubid), '; ' ORDER BY 1)
         FROM pg_depend d
         WHERE d.deptype = 'e'
           AND d.refclassid = 'pg_extension'::regclass
           AND d.refobjid = (SELECT oid FROM pg_extension WHERE extname = 'pg_automerge')
           AND d.classid IN ('pg_proc'::regclass, 'pg_type'::regclass,
                             'pg_operator'::regclass, 'pg_cast'::regclass)
           AND NOT (d.classid = 'pg_proc'::regclass AND EXISTS (
               SELECT 1 FROM pg_proc p
               WHERE p.oid = d.objid AND p.pronamespace = 'tests'::regnamespace))
           AND NOT (d.classid = 'pg_type'::regclass AND EXISTS (
               SELECT 1 FROM pg_type t WHERE t.typarray = d.objid))
           AND obj_description(d.objid, d.classid::regclass::text) IS NULL",
    )
    .unwrap();
    assert_eq!(uncommented, None, "objects without a COMMENT");
    // The check sees every kind of object.
    let kinds: String = one(
        "SELECT string_agg(DISTINCT d.classid::regclass::text, ',')
         FROM pg_depend d
         WHERE d.deptype = 'e'
           AND d.refobjid = (SELECT oid FROM pg_extension WHERE extname = 'pg_automerge')",
        &[],
    );
    for kind in ["pg_cast", "pg_operator", "pg_proc", "pg_type"] {
        assert!(kinds.contains(kind), "{kinds}");
    }
}

#[pg_test]
fn type_oid_is_the_extensions_type_whatever_the_search_path() {
    let ours: pg_sys::Oid = one(
        "SELECT t.oid FROM pg_type t JOIN pg_extension e ON e.extnamespace = t.typnamespace \
         WHERE e.extname = 'pg_automerge' AND t.typname = 'automerge'",
        &[],
    );
    // A type of the same name earlier on search_path, and the extension's
    // schema off it: the Rust types still name the extension's type.
    Spi::run("CREATE SCHEMA tn_evil; CREATE TYPE tn_evil.automerge AS (x int)").unwrap();
    Spi::run("SET LOCAL search_path TO tn_evil, pg_catalog").unwrap();
    assert_eq!(crate::datum::AutomergeValue::type_oid(), ours);
    assert_eq!(crate::datum::AutomergeDatum::type_oid(), ours);
    Spi::run("RESET search_path").unwrap();
}
