// Documents with deeply nested text blocks: every function reads them
// without overflowing the stack (Automerge's own rendering of such a
// block, which the jsonb view once reached through its document iterator,
// recurses about 13 kB per level: 5,000 levels need some 65 MB). See
// docs/DESIGN.md, "Deep blocks".

/// {text: "ab" with a block at 1 holding maps nested `depth` levels} in a
/// first change, then {status: "new"} in a second one.
fn deep_block_doc(depth: usize) -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    doc.splice_text(&text, 0, 0, "ab").unwrap();
    let mut obj = doc.split_block(&text, 1).unwrap();
    for _ in 0..depth {
        obj = doc.put_object(&obj, "m", ObjType::Map).unwrap();
    }
    doc.commit();
    doc.put(ROOT, "status", "new").unwrap();
    doc.commit();
    doc
}

#[pg_test]
fn deep_blocks_read_as_jsonb() {
    let mut doc = deep_block_doc(5000);
    let bytes = stored(&mut doc);
    let first = doc.get_changes(&[])[0].hash().to_string();
    let expected = json!({"text": "a\u{fffc}b", "status": "new"});
    Spi::run(
        "CREATE TEMP TABLE deep_docs (id int PRIMARY KEY, doc automerge NOT NULL, \
         data jsonb GENERATED ALWAYS AS (doc::jsonb) STORED)",
    )
    .unwrap();
    Spi::run("CREATE INDEX deep_docs_expr ON deep_docs USING gin ((doc::jsonb))").unwrap();
    Spi::run("CREATE INDEX deep_docs_status ON deep_docs ((doc->>'status'))").unwrap();
    // The generated column and both indexes compute the jsonb on insert.
    Spi::run_with_args(
        "INSERT INTO deep_docs (id, doc) VALUES (1, $1)",
        &[bytes.clone().into()],
    )
    .unwrap();
    let data: JsonB = one("SELECT data FROM deep_docs WHERE id = 1", &[]);
    assert_eq!(data.0, expected);
    // Stored, expanded (the bytea cast), a merge result and merge_agg.
    for q in [
        "SELECT doc::jsonb FROM deep_docs WHERE id = 1",
        "SELECT automerge_to_jsonb(doc) FROM deep_docs WHERE id = 1",
        "SELECT $1::bytea::automerge::jsonb",
        "SELECT merge_agg(doc)::jsonb FROM deep_docs",
        "SELECT merge(doc, $1::bytea::automerge)::jsonb FROM deep_docs WHERE id = 1",
    ] {
        let got: JsonB = one(q, &[bytes.clone().into()]);
        assert_eq!(got.0, expected, "{q}");
    }
    let mut other = AutoCommit::new().with_actor(actor(2));
    other.put(ROOT, "other", 1).unwrap();
    let got: JsonB = one(
        "SELECT merge(doc, $1)::jsonb FROM deep_docs WHERE id = 1",
        &[other.save().into()],
    );
    assert_eq!(got.0["text"], "a\u{fffc}b");
    assert_eq!(got.0["other"], 1);
    // As of the first change (the block and its nesting) and of now.
    let got: JsonB = one(
        "SELECT automerge_to_jsonb(doc, ARRAY[$1]) FROM deep_docs WHERE id = 1",
        &[first.clone().into()],
    );
    assert_eq!(got.0, json!({"text": "a\u{fffc}b"}));
    let got: JsonB = one(
        "SELECT automerge_to_jsonb(doc, automerge_heads(doc)) FROM deep_docs WHERE id = 1",
        &[],
    );
    assert_eq!(got.0, expected);
    // Accessors and the expression index.
    let status: String = one("SELECT doc->>'status' FROM deep_docs WHERE id = 1", &[]);
    assert_eq!(status, "new");
    Spi::run("SET LOCAL enable_seqscan = off").unwrap();
    let id: i32 = one(
        "SELECT id FROM deep_docs WHERE doc::jsonb @> '{\"status\": \"new\"}'",
        &[],
    );
    assert_eq!(id, 1);
    // Updates recompute the generated column.
    Spi::run_with_args(
        "UPDATE deep_docs SET doc = merge(doc, $1) WHERE id = 1",
        &[other.save().into()],
    )
    .unwrap();
    let other_value: i64 = one("SELECT (data->>'other')::bigint FROM deep_docs", &[]);
    assert_eq!(other_value, 1);
}

#[pg_test]
fn deep_blocks_keep_every_other_function() {
    let mut doc = deep_block_doc(5000);
    let bytes = stored(&mut doc);
    Spi::run("CREATE TEMP TABLE deep_rt (doc automerge)").unwrap();
    Spi::run_with_args("INSERT INTO deep_rt VALUES ($1)", &[bytes.clone().into()]).unwrap();
    // Text and binary round trips, history, containment.
    let same: bool = one("SELECT doc::text::automerge = doc FROM deep_rt", &[]);
    assert!(same);
    let sent: Vec<u8> = one("SELECT automerge_send(doc) FROM deep_rt", &[]);
    assert_eq!(sent, bytes);
    let n: i64 = one("SELECT automerge_change_count(doc) FROM deep_rt", &[]);
    assert_eq!(n, 2);
    let n: i64 = one("SELECT count(*) FROM deep_rt, automerge_changes(doc)", &[]);
    assert_eq!(n, 2);
    let n: i64 = one("SELECT count(*) FROM deep_rt, automerge_changes_meta(doc)", &[]);
    assert_eq!(n, 2);
    let contains: bool = one("SELECT automerge_contains(doc, $1::bytea::automerge) FROM deep_rt", &[
        bytes.clone().into(),
    ]);
    assert!(contains);
    let len: i32 = one(
        "SELECT length(automerge_changes_bytes(doc)) FROM deep_rt",
        &[],
    );
    assert!(len > 0);
    // Spans of that text are refused before Automerge renders the block;
    // the other texts' spans are unaffected.
    let report = sql_error_report("SELECT automerge_spans(doc, '{text}') FROM deep_rt");
    assert_eq!(
        report[..2],
        [
            "54000".to_owned(),
            format!(
                "automerge text block is nested more than {} levels deep",
                am::spans::MAX_BLOCK_DEPTH
            )
        ]
    );
}
