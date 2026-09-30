// automerge_spans(automerge, text[]) and automerge_spans(automerge, text[], text[]).

/// {notes: [{body: Text "first" (bold "fir"), n: 1}, {body: Text "second" + a block}],
///  title: Text "Title", status: "open", count: counter, items: []}.
fn spans_doc() -> AutoCommit {
    use pg_automerge_core::automerge::marks::{ExpandMark, Mark};
    let mut doc = AutoCommit::new().with_actor(actor(1));
    let notes = doc.put_object(ROOT, "notes", ObjType::List).unwrap();
    for (i, s) in ["first", "second"].into_iter().enumerate() {
        let note = doc.insert_object(&notes, i, ObjType::Map).unwrap();
        let body = doc.put_object(&note, "body", ObjType::Text).unwrap();
        doc.splice_text(&body, 0, 0, s).unwrap();
        doc.put(&note, "n", i as i64 + 1).unwrap();
        if i == 0 {
            doc.mark(&body, Mark::new("bold".into(), true, 0, 3), ExpandMark::After)
                .unwrap();
        } else {
            let b = doc.split_block(&body, 0).unwrap();
            doc.put(&b, "type", "heading").unwrap();
            let attrs = doc.put_object(&b, "attrs", ObjType::Map).unwrap();
            doc.put(&attrs, "level", 2i64).unwrap();
        }
    }
    let title = doc.put_object(ROOT, "title", ObjType::Text).unwrap();
    doc.splice_text(&title, 0, 0, "Title").unwrap();
    doc.put(ROOT, "status", "open").unwrap();
    doc.put(
        ROOT,
        "count",
        pg_automerge_core::automerge::ScalarValue::counter(3),
    )
    .unwrap();
    doc.put_object(ROOT, "items", ObjType::List).unwrap();
    doc
}

/// `automerge_spans($1, path)` as jsonb text (None for NULL).
fn spans_sql(bytes: &[u8], path: &str) -> Option<JsonB> {
    Spi::get_one_with_args::<JsonB>(
        &format!("SELECT automerge_spans($1::bytea::automerge, {path})"),
        &[bytes.into()],
    )
    .unwrap()
}

#[pg_test]
fn spans_resolve_paths_like_jsonb() {
    let bytes = spans_doc().save();
    let first = json!([
        {"type": "text", "value": "fir", "marks": {"bold": true}},
        {"type": "text", "value": "st"},
    ]);
    let second = json!([
        {"type": "block", "value": {"type": "heading", "attrs": {"level": 2}}},
        {"type": "text", "value": "second"},
    ]);
    for (path, expected) in [
        ("'{notes,0,body}'", Some(first.clone())),
        ("'{notes,1,body}'", Some(second.clone())),
        ("'{notes,-1,body}'", Some(second.clone())),
        ("ARRAY['notes', '+0', 'body']", Some(first.clone())),
        ("'{title}'", Some(json!([{"type": "text", "value": "Title"}]))),
        // Nothing there: NULL, as for #>.
        ("'{notes,2,body}'", None),
        ("'{notes,-3,body}'", None),
        ("'{notes,x,body}'", None),
        ("'{notes,0,nope}'", None),
        ("'{nope}'", None),
        ("'{status,x}'", None),
        ("'{title,0}'", None),
        ("ARRAY['notes', NULL, 'body']", None),
    ] {
        let got = spans_sql(&bytes, path).map(|j| j.0);
        assert_eq!(got, expected, "{path}");
        // #> agrees on which paths exist (it shows text as a string).
        let jsonb_null: bool = one(
            &format!("SELECT ($1::bytea::automerge #> {path}) IS NULL"),
            &[bytes.clone().into()],
        );
        assert_eq!(jsonb_null, expected.is_none(), "{path}");
    }
    // Something that is not a text object: 22023, naming the path and the
    // type.
    for (path, what) in [
        ("'{}'", "{} is a map"),
        ("'{notes}'", "{notes} is a list"),
        ("'{notes,0}'", "{notes,0} is a map"),
        ("'{notes,0,n}'", "{notes,0,n} is an integer"),
        ("'{status}'", "{status} is a string scalar"),
        ("'{count}'", "{count} is a counter"),
        ("'{items}'", "{items} is a list"),
    ] {
        let report = sql_error_report(&format!(
            "SELECT automerge_spans('{}'::bytea::automerge, {path})",
            hex_literal(&bytes)
        ));
        assert_eq!(
            report,
            vec![
                "22023".to_owned(),
                format!("automerge value at path {what}, not a text object"),
                String::new(),
                String::new(),
            ],
            "{path}"
        );
    }
}

fn hex_literal(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::from("\\x");
    for b in bytes {
        write!(s, "{b:02x}").unwrap();
    }
    s
}

#[pg_test]
fn spans_match_the_core_and_accept_expanded_values() {
    let mut doc = spans_doc();
    let bytes = doc.save();
    let stored = doc.document().save_nocompress();
    // Another replica's change: merging it gives an expanded value.
    let mut other = doc.fork().with_actor(actor(2));
    other.put(ROOT, "other", true).unwrap();
    let other = other.save();
    for path in [vec!["notes", "0", "body"], vec!["notes", "1", "body"], vec!["title"]] {
        let expected = am::spans::spans_to_json(am::loaded::Input::Stored(&stored), &path, None)
            .unwrap()
            .unwrap();
        let text = serde_json::to_string(&expected).unwrap();
        let path_sql: Vec<String> = path.iter().map(|s| s.to_string()).collect();
        let same: bool = one(
            "SELECT automerge_spans($1::bytea::automerge, $2) = $3::jsonb \
               AND automerge_spans(merge($1::bytea::automerge, $4::bytea::automerge), $2) = $3::jsonb \
               AND automerge_spans($1::bytea::automerge, $2, automerge_heads($1::bytea::automerge)) = $3::jsonb",
            &[bytes.clone().into(), path_sql.into(), text.into(), other.clone().into()],
        );
        assert!(same, "{path:?}");
    }
    // An expanded value (a merge result kept loaded) in PL/pgSQL.
    Spi::run(
        "CREATE FUNCTION pg_temp.spans_of_merge(a automerge, b automerge) RETURNS jsonb \
         LANGUAGE plpgsql AS $$ DECLARE d automerge; BEGIN d := merge(a, b); \
         RETURN automerge_spans(d, '{notes,0,body}'); END $$",
    )
    .unwrap();
    let v: JsonB = one(
        "SELECT pg_temp.spans_of_merge($1::bytea::automerge, $2::bytea::automerge)",
        &[bytes.into(), other.into()],
    );
    assert_eq!(
        v.0,
        json!([{"type": "text", "value": "fir", "marks": {"bold": true}}, {"type": "text", "value": "st"}])
    );
}

#[pg_test]
fn spans_at_heads() {
    use pg_automerge_core::automerge::marks::{ExpandMark, Mark};
    let mut doc = AutoCommit::new().with_actor(actor(1));
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    doc.splice_text(&text, 0, 0, "Hello").unwrap();
    doc.commit();
    let plain = heads_of(&mut doc);
    doc.mark(&text, Mark::new("em".into(), true, 0, 5), ExpandMark::After)
        .unwrap();
    doc.commit();
    let bytes = doc.save();
    let args = || -> Vec<DatumWithOid<'static>> { vec![bytes.clone().into(), plain.clone().into()] };
    let got: JsonB = one(
        "SELECT automerge_spans($1::bytea::automerge, '{text}', $2)",
        &args(),
    );
    assert_eq!(got.0, json!([{"type": "text", "value": "Hello"}]));
    let got: JsonB = one(
        "SELECT automerge_spans($1::bytea::automerge, '{text}', automerge_heads($1::bytea::automerge))",
        &args(),
    );
    assert_eq!(got.0, json!([{"type": "text", "value": "Hello", "marks": {"em": true}}]));
    // Before any change: the root is empty, nothing at the path.
    let none = Spi::get_one_with_args::<JsonB>(
        "SELECT automerge_spans($1::bytea::automerge, '{text}', '{}')",
        &args(),
    )
    .unwrap();
    assert!(none.is_none());
    // Heads errors, as for automerge_to_jsonb(doc, heads).
    let empty = "''::bytea::automerge";
    assert_eq!(
        sql_error(&format!(
            "SELECT automerge_spans({empty}, '{{text}}', ARRAY[repeat('ab', 32)])"
        )),
        format!("22023: automerge document does not contain change {}", "ab".repeat(32))
    );
    assert!(
        sql_error(&format!("SELECT automerge_spans({empty}, '{{text}}', ARRAY['abc'])"))
            .starts_with("22P02: invalid automerge change hash \"abc\""),
    );
    assert_eq!(
        sql_error(&format!(
            "SELECT automerge_spans({empty}, '{{text}}', ARRAY[NULL]::text[])"
        )),
        "22004: heads must not contain NULL"
    );
    // STRICT: any NULL argument gives NULL.
    for q in [
        "SELECT automerge_spans(NULL, '{text}')",
        "SELECT automerge_spans(''::bytea::automerge, NULL)",
        "SELECT automerge_spans(''::bytea::automerge, '{text}', NULL)",
        "SELECT automerge_spans(NULL, NULL, NULL)",
    ] {
        assert!(Spi::get_one::<JsonB>(q).unwrap().is_none(), "{q}");
    }
}

#[pg_test]
fn spans_refuse_deep_blocks() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    doc.splice_text(&text, 0, 0, "ab").unwrap();
    let mut obj = doc.split_block(&text, 1).unwrap();
    for _ in 0..5000 {
        obj = doc.put_object(&obj, "m", ObjType::Map).unwrap();
    }
    let report = sql_error_report(&format!(
        "SELECT automerge_spans('{}'::bytea::automerge, '{{text}}')",
        hex_literal(&doc.save())
    ));
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

#[pg_test]
fn spans_functions_are_labelled() {
    let rows: Vec<String> = one(
        "SELECT array_agg(p.oid::regprocedure::text || ' ' || pg_get_function_arguments(p.oid) || ' ' \
                || provolatile::text || proisstrict::text || proparallel::text || ' ' \
                || prorettype::regtype::text ORDER BY pronargs) \
         FROM pg_proc p WHERE proname = 'automerge_spans'",
        &[],
    );
    assert_eq!(
        rows,
        vec![
            "automerge_spans(automerge,text[]) doc automerge, path text[] itrues jsonb",
            "automerge_spans(automerge,text[],text[]) doc automerge, path text[], heads text[] itrues jsonb",
        ]
    );
}
