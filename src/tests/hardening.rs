// Hardening: jsonb built directly is byte-identical to the jsonb_in path;
// the deferred save-and-load check (forced to fail by a test hook) rejects
// at flatten time and leaves rows and variables intact; the jsonb walk
// honours query cancel.

/// `tests.fail_reload_check(bool)`: make the save-and-load check of this
/// backend fail (true) or work normally (false). Test builds only.
#[pg_extern]
fn fail_reload_check(fail: bool) {
    am::test_hooks::set_fail_reload_check(fail);
}

/// Resets the hook when a test ends, also when it fails.
struct ReloadCheckFails;

impl ReloadCheckFails {
    fn on() -> Self {
        am::test_hooks::set_fail_reload_check(true);
        Self
    }
}

impl Drop for ReloadCheckFails {
    fn drop(&mut self) {
        am::test_hooks::set_fail_reload_check(false);
    }
}

/// The bytes of a jsonb datum (detoasted, without the varlena header).
fn jsonb_datum_bytes(datum: pg_sys::Datum) -> Vec<u8> {
    // SAFETY: a jsonb datum built in this call.
    unsafe {
        let ptr = pg_sys::pg_detoast_datum_packed(datum.cast_mut_ptr());
        let len = pgrx::varlena::varsize_any_exhdr(ptr);
        std::slice::from_raw_parts(pgrx::varlena::vardata_any(ptr).cast::<u8>(), len).to_vec()
    }
}

/// jsonb of the document, built directly (the path of `::jsonb`).
fn direct_jsonb(
    doc: &am::automerge::Automerge,
    heads: Option<&[am::automerge::ChangeHash]>,
) -> Vec<u8> {
    let mut builder = crate::jsonb::JsonbBuilder::default();
    am::json::write_json_at(doc, heads, &mut builder).unwrap();
    jsonb_datum_bytes(builder.finish().unwrap().into_datum().unwrap())
}

/// jsonb of the document through JSON text and `jsonb_in` (the path
/// before jsonb was built directly: pgrx's `JsonB`).
fn text_jsonb(
    doc: &am::automerge::Automerge,
    heads: Option<&[am::automerge::ChangeHash]>,
) -> Vec<u8> {
    let value = am::json::doc_to_json_at(doc, heads).unwrap();
    jsonb_datum_bytes(JsonB(value).into_datum().unwrap())
}

#[pg_test]
fn direct_jsonb_is_byte_identical_to_jsonb_in() {
    use pg_automerge_core::automerge::ScalarValue;
    let mut doc = sample();
    let floats = [
        0.0,
        -0.0,
        0.1,
        0.1 + 0.2,
        -1.5,
        1e15,
        1e16,
        1e21,
        1e300,
        1e-7,
        1e-300,
        5e-324,
        f64::MAX,
        f64::MIN_POSITIVE,
        9_007_199_254_740_993.0,
        123_456_789.123_456_79,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ];
    let floats_list = doc.put_object(ROOT, "floats", ObjType::List).unwrap();
    for (i, f) in floats.into_iter().enumerate() {
        doc.insert(&floats_list, i, f).unwrap();
    }
    let ints = doc.put_object(ROOT, "ints", ObjType::Map).unwrap();
    for (k, v) in [
        ("imin", ScalarValue::Int(i64::MIN)),
        ("imax", ScalarValue::Int(i64::MAX)),
        ("zero", ScalarValue::Int(0)),
        ("minus", ScalarValue::Int(-1)),
        ("umax", ScalarValue::Uint(u64::MAX)),
        ("ubig", ScalarValue::Uint(i64::MAX as u64 + 1)),
        ("usmall", ScalarValue::Uint(7)),
        ("counter", ScalarValue::counter(-42)),
        ("ts", ScalarValue::Timestamp(-62_167_219_200_001)),
        ("bytes", ScalarValue::Bytes(vec![0, 1, 0xfe, 0xff])),
        ("null", ScalarValue::Null),
        ("t", ScalarValue::Boolean(true)),
        ("f", ScalarValue::Boolean(false)),
    ] {
        doc.put(&ints, k, v).unwrap();
    }
    for (k, v) in [
        ("", "empty key"),
        ("empty", ""),
        ("quotes", "\"\\/\u{1}\u{1f}\t\n"),
        ("😀", "a👩‍👩‍👧‍👦b"),
        // Two keys that are equal after U+0000 becomes U+FFFD: one wins.
        ("dup\0key", "from nul"),
        ("dup\u{FFFD}key", "from replacement"),
    ] {
        doc.put(ROOT, k, v).unwrap();
    }
    doc.put(ROOT, "long key ".repeat(100), "x".repeat(10_000))
        .unwrap();
    doc.put_object(ROOT, "empty_map", ObjType::Map).unwrap();
    let nested = doc.put_object(ROOT, "nested", ObjType::List).unwrap();
    let inner = doc.insert_object(&nested, 0, ObjType::List).unwrap();
    doc.insert_object(&inner, 0, ObjType::List).unwrap();
    doc.insert_object(&nested, 1, ObjType::Map).unwrap();
    doc.commit();
    let old_heads = doc.get_heads();
    doc.put(ROOT, "later", 1i64).unwrap();
    // A concurrent conflict.
    let mut other = doc.fork().with_actor(actor(7));
    other.put(ROOT, "status", "theirs").unwrap();
    doc.put(ROOT, "status", "ours").unwrap();
    doc.merge(&mut other).unwrap();

    let loaded = doc.document().clone();
    for heads in [None, Some(old_heads.as_slice()), Some(&[][..])] {
        assert_eq!(direct_jsonb(&loaded, heads), text_jsonb(&loaded, heads));
    }
    for seed in 0..20u64 {
        let mut d = AutoCommit::new().with_actor(actor(seed as u8 + 1));
        let mut rng = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        for i in 0..200 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let key = format!("k{}", rng % 37);
            match rng % 5 {
                0 => d.put(ROOT, key, (rng >> 11) as f64 / 7.0).unwrap(),
                1 => d.put(ROOT, key, rng as i64).unwrap(),
                2 => d.put(ROOT, key, format!("v{i}")).unwrap(),
                3 => {
                    d.put_object(ROOT, key, ObjType::List).unwrap();
                }
                _ => d.put(ROOT, key, rng % 2 == 0).unwrap(),
            }
        }
        let loaded = d.document().clone();
        assert_eq!(direct_jsonb(&loaded, None), text_jsonb(&loaded, None));
    }
    // And through SQL: both jsonb functions against the text path.
    let text = serde_json::to_string(&am::json::doc_to_json(&loaded).unwrap()).unwrap();
    let same: bool = one(
        "SELECT $1::automerge::jsonb = $2::jsonb \
           AND automerge_to_jsonb($1::automerge, automerge_heads($1::automerge)) = $2::jsonb",
        &[doc.save().into(), text.into()],
    );
    assert!(same);
}

#[pg_test]
fn deferred_verification_failure_rejects_at_flatten_time() {
    let mut base = sample();
    base.commit();
    let base_bytes = stored(&mut base);
    let mut writer = base.fork().with_actor(actor(5));
    let heads = writer.get_heads();
    writer.put(ROOT, "status", "done").unwrap();
    writer.commit();
    let changes = writer.save_after(&heads);
    let merged = stored_merge_changes(&base_bytes, &changes);
    Spi::run("CREATE TEMP TABLE dv (id int PRIMARY KEY, doc automerge NOT NULL)").unwrap();
    Spi::run_with_args(
        "INSERT INTO dv VALUES (1, $1::automerge)",
        &[base_bytes.clone().into()],
    )
    .unwrap();
    Spi::run_with_args(
        "CREATE TEMP TABLE dv_changes AS SELECT $1::bytea AS c",
        &[changes.clone().into()],
    )
    .unwrap();

    let failing = ReloadCheckFails::on();
    let before = am::test_hooks::reload_checks();
    // Merging and reading never flatten: no check, no error.
    let status: String = one("SELECT merge(doc, c)->>'status' FROM dv, dv_changes", &[]);
    assert_eq!(status, "done");
    let heads: Vec<String> = one(
        "SELECT automerge_heads(merge(doc, c)) FROM dv, dv_changes",
        &[],
    );
    assert_eq!(heads, am::heads_to_strings(writer.get_heads()));
    assert_eq!(am::test_hooks::reload_checks(), before);
    // Storing, casting to bytea and sending do, and fail with 22P02.
    let expected = "22P02: invalid automerge changes: does not survive a save and load \
                    (forced by a test hook)";
    for sql in [
        "UPDATE dv SET doc = merge(doc, c) FROM dv_changes",
        "SELECT length(merge(doc, c)::bytea) FROM dv, dv_changes",
        "SELECT automerge_send(merge(doc, c)) FROM dv, dv_changes",
        "INSERT INTO dv SELECT 2, merge(doc, c) FROM dv, dv_changes",
    ] {
        assert_eq!(sql_error(sql), expected, "{sql}");
    }
    // The row is unchanged.
    let unchanged: bool = one(
        "SELECT doc::bytea = $1 FROM dv WHERE id = 1",
        &[base_bytes.clone().into()],
    );
    assert!(unchanged);
    let rows: i64 = one("SELECT count(*) FROM dv", &[]);
    assert_eq!(rows, 1);

    // A PL/pgSQL variable holding the merge result stays usable after the
    // failed UPDATE, and stores fine once the check passes.
    Spi::run(
        "DO $$
         DECLARE d automerge; ch bytea; failed bool := false;
         BEGIN
             SELECT doc INTO d FROM dv WHERE id = 1;
             SELECT c INTO ch FROM dv_changes;
             d := merge(d, ch);
             BEGIN
                 UPDATE dv SET doc = d WHERE id = 1;
             EXCEPTION WHEN invalid_text_representation THEN
                 failed := true;
             END;
             ASSERT failed, 'the UPDATE should have failed';
             ASSERT d->>'status' = 'done', 'variable unusable';
             ASSERT cardinality(automerge_heads(d)) = 1, 'heads';
             PERFORM tests.fail_reload_check(false);
             UPDATE dv SET doc = d WHERE id = 1;
         END $$",
    )
    .unwrap();
    let stored_ok: bool = one(
        "SELECT doc::bytea = $1 FROM dv WHERE id = 1",
        &[merged.into()],
    );
    assert!(stored_ok);
    drop(failing);
}

#[pg_test]
fn input_verification_failure_and_the_compressed_shortcut() {
    let mut doc = sample();
    doc.commit();
    let heads = doc.get_heads();
    let compressed = doc.save();
    let canonical = stored(&mut doc);
    doc.put(ROOT, "later", true).unwrap();
    doc.commit();
    let trailing = [compressed.as_slice(), &doc.save_after(&heads)].concat();
    let failing = ReloadCheckFails::on();
    // Input that needs the save-and-load check is rejected when it fails.
    let hex = am::encoding::to_hex_literal(&trailing);
    assert_eq!(
        sql_error(&format!("SELECT '{hex}'::automerge")),
        "22P02: invalid automerge document: does not survive a save and load \
         (forced by a test hook)"
    );
    assert!(sql_error(&format!("SELECT '{hex}'::bytea::automerge")).starts_with("22P02: "));
    drop(failing);
    // A real input that loads but whose re-save does not (found by the
    // core's fuzz harness): rejected by the check itself.
    let real = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/crates/pg_automerge_core/tests/corpus/reload-b-84714.bin"
    ));
    assert_eq!(
        sql_error(&format!(
            "SELECT '{}'::automerge",
            am::encoding::to_hex_literal(real)
        )),
        "22P02: invalid automerge document: does not survive a save and load \
         (error inflating document chunk ops: mismatching heads)"
    );
    let _failing = ReloadCheckFails::on();
    // Canonical bytes and a compressed save of one document never need
    // it (the compressed save's inflated form is the canonical save).
    let before = am::test_hooks::reload_checks();
    for input in [canonical.clone(), compressed] {
        let ok: bool = one(
            "SELECT $1::automerge::bytea = $2",
            &[input.into(), canonical.clone().into()],
        );
        assert!(ok);
    }
    assert_eq!(am::test_hooks::reload_checks(), before);
}

#[pg_test(error = "canceling statement due to user request")]
fn jsonb_walk_honours_query_cancel() {
    let mut doc = AutoCommit::new();
    let list = doc.put_object(ROOT, "items", ObjType::List).unwrap();
    for i in 0..4 * am::TICK_EVERY as usize {
        doc.insert(&list, i, i as i64).unwrap();
    }
    let bytes = stored(&mut doc);
    // A cancel request arrives: nothing but the walk's own interrupt
    // checks runs between here and the end of the walk, so the ERROR can
    // only come from inside it.
    unsafe {
        pg_sys::QueryCancelPending = 1;
        pg_sys::InterruptPending = 1;
    }
    let mut builder = crate::jsonb::JsonbBuilder::default();
    let result = am::loaded::with_doc(am::loaded::Input::Stored(&bytes), |d| {
        am::json::write_json_at(d, None, &mut builder)
    });
    // Not reached.
    unsafe {
        pg_sys::QueryCancelPending = 0;
        pg_sys::InterruptPending = 0;
    }
    panic!("the walk finished: {:?}", result.map(|_| ()));
}
