// Loads per write path (counted by the core's test hook): full saves
// through merge(automerge, bytea) and the bytea cast load only the save,
// the cast's expanded value is released per row, and the
// pg_automerge.verify_writes setting.

/// `Automerge::load` calls of this backend so far.
fn loads() -> usize {
    am::test_hooks::loads()
}

/// Put the stored base document back into `ld` (SPI allows no savepoints).
fn reset_ld() {
    Spi::run("DELETE FROM ld WHERE id <> 1").unwrap();
    Spi::run("UPDATE ld SET doc = (SELECT b FROM ld_in WHERE name = 'base')::automerge").unwrap();
}

/// Loads taken by running `sql`.
fn loads_of(sql: &str) -> usize {
    let before = loads();
    Spi::run(sql).unwrap();
    loads() - before
}

/// A stored base document and, as a backend sends them, a newer version
/// (one more change by another actor) as a compressed full save and as the
/// changes since the base; plus an older save (the base before its last
/// change) and a concurrent save (a change the newer one does not have).
/// Loaded into temp tables `ld(id, doc)` and `ld_in(name, b)`.
fn loads_fixture() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut doc = sample();
    doc.commit();
    let older = doc.save();
    doc.put(ROOT, "status", "stored").unwrap();
    doc.commit();
    let base = am::normalize(&doc.save()).unwrap();
    let mut newer = AutoCommit::load(&doc.save()).unwrap().with_actor(actor(2));
    let heads = newer.get_heads();
    newer.put(ROOT, "status", "edited").unwrap();
    newer.commit();
    let newer_save = newer.save();
    let changes = newer.save_after(&heads);
    // Forked before the stored document's last change: neither contains
    // the other.
    let mut other = AutoCommit::load(&older).unwrap().with_actor(actor(3));
    other.put(ROOT, "other", true).unwrap();
    other.commit();
    Spi::run("CREATE TEMP TABLE ld (id int PRIMARY KEY, doc automerge NOT NULL)").unwrap();
    Spi::run_with_args("INSERT INTO ld VALUES (1, $1::automerge)", &[base.clone().into()]).unwrap();
    Spi::run("CREATE TEMP TABLE ld_in (name text PRIMARY KEY, b bytea NOT NULL)").unwrap();
    for (name, bytes) in [
        ("newer", newer_save.clone()),
        ("changes", changes),
        ("older", older),
        ("base", base.clone()),
        ("concurrent", other.save()),
    ] {
        Spi::run_with_args(
            "INSERT INTO ld_in VALUES ($1, $2)",
            &[name.into(), bytes.into()],
        )
        .unwrap();
    }
    (base, newer_save, am::normalize(&newer.save()).unwrap())
}

#[pg_test]
fn merging_a_full_save_loads_only_the_save() {
    let (base, _, newer_stored) = loads_fixture();
    let update = |name: &str| {
        format!("UPDATE ld SET doc = merge(doc, (SELECT b FROM ld_in WHERE name = '{name}'))")
    };
    // A newer save contains the stored document: the save is loaded once,
    // its own (compressed) encoding proves its save loads back, and it is
    // the result; the stored document is never loaded.
    let checks = am::test_hooks::reload_checks();
    assert_eq!(loads_of(&update("newer")), 1);
    assert_eq!(am::test_hooks::reload_checks(), checks);
    let same: bool = one("SELECT doc::bytea = $1 FROM ld", &[newer_stored.clone().into()]);
    assert!(same);
    reset_ld();
    // The same save again, or the stored bytes themselves: the header
    // decides, nothing is loaded.
    assert_eq!(loads_of(&update("base")), 0);
    Spi::run("UPDATE ld SET doc = merge(doc, (SELECT b FROM ld_in WHERE name = 'newer'))")
        .unwrap();
    assert_eq!(loads_of(&update("newer")), 0);
    reset_ld();
    // An older save (fewer changes): the stored document is loaded and
    // has its heads; the save is not loaded.
    assert_eq!(loads_of(&update("older")), 1);
    let same: bool = one("SELECT doc::bytea = $1 FROM ld", &[base.clone().into()]);
    assert!(same);
    // A concurrent save: both are loaded and merged, and the result is
    // checked when stored.
    let checks = am::test_hooks::reload_checks();
    assert_eq!(loads_of(&update("concurrent")), 3);
    assert_eq!(am::test_hooks::reload_checks(), checks + 1);
    let state: serde_json::Value = one::<JsonB>("SELECT doc::jsonb FROM ld", &[]).0;
    assert_eq!(state["status"], json!("stored"));
    assert_eq!(state["other"], json!(true));
    reset_ld();
    // Incremental changes: the stored document, and the check.
    assert_eq!(loads_of(&update("changes")), 2);
    // automerge_contains(doc, save): the header heads against the stored
    // document, never the save.
    reset_ld();
    for (name, contained, n) in [("newer", false, 1), ("older", true, 1), ("base", true, 0)] {
        let before = loads();
        let got: bool = one(
            &format!(
                "SELECT automerge_contains(doc, (SELECT b FROM ld_in WHERE name = '{name}')) FROM ld"
            ),
            &[],
        );
        assert_eq!((got, loads() - before), (contained, n), "{name}");
    }
}

#[pg_test]
fn the_bytea_cast_hands_its_document_to_merge() {
    let (_, newer_save, newer_stored) = loads_fixture();
    // The cast loads the save once; merge reads the loaded document; the
    // UPDATE stores its bytes (no save, no check).
    let checks = am::test_hooks::reload_checks();
    assert_eq!(
        loads_of(
            "UPDATE ld SET doc = merge(doc, (SELECT b FROM ld_in WHERE name = 'newer')::automerge)"
        ),
        1
    );
    assert_eq!(am::test_hooks::reload_checks(), checks);
    let same: bool = one("SELECT doc::bytea = $1 FROM ld", &[newer_stored.clone().into()]);
    assert!(same);
    reset_ld();
    // An upsert: EXCLUDED arrives flat (ExecInsert materializes the
    // proposed tuple before the conflict check), so merge loads it again.
    assert_eq!(
        loads_of(
            "INSERT INTO ld VALUES (1, (SELECT b FROM ld_in WHERE name = 'newer')) \
             ON CONFLICT (id) DO UPDATE SET doc = merge(ld.doc, excluded.doc)"
        ),
        2
    );
    let same: bool = one("SELECT doc::bytea = $1 FROM ld", &[newer_stored.clone().into()]);
    assert!(same);
    reset_ld();
    // A plain INSERT of the cast, reads of it, its text and binary output,
    // and a bytea parameter (a custom plan folds the cast into a flat
    // constant): one load each, the same bytes as before.
    assert_eq!(
        loads_of("INSERT INTO ld SELECT 2, b FROM ld_in WHERE name = 'newer'"),
        1
    );
    let before = loads();
    let (status, sent, cast): (Option<String>, Option<Vec<u8>>, Option<Vec<u8>>) =
        Spi::get_three(
            "SELECT x->>'status', automerge_send(x), x::bytea
             FROM (SELECT b::automerge AS x FROM ld_in WHERE name = 'newer' OFFSET 0) s",
        )
        .unwrap();
    assert_eq!(loads() - before, 1);
    assert_eq!(status.as_deref(), Some("edited"));
    assert_eq!(sent.as_deref(), Some(newer_stored.as_slice()));
    assert_eq!(cast.as_deref(), Some(newer_stored.as_slice()));
    let text: String = one("SELECT (b::automerge)::text FROM ld_in WHERE name = 'newer'", &[]);
    assert_eq!(text, am::encoding::to_hex_literal(&newer_stored));
    let same: bool = one(
        "SELECT $1::automerge::bytea = $2",
        &[newer_save.into(), newer_stored.into()],
    );
    assert!(same);
    reset_ld();
}

#[pg_test]
fn cast_values_are_released_row_by_row() {
    loads_fixture();
    let live_before = am::loaded::live_count();
    assert_eq!(expanded_contexts(), 0);
    // While the statement runs, at most the current row's value exists.
    let most: i64 = one(
        "SELECT max(c) FROM (
             SELECT cardinality(automerge_heads(b::automerge)) AS h,
                    (SELECT count(*) FROM pg_backend_memory_contexts
                     WHERE name = 'automerge expanded document' AND g > 0) AS c
             FROM generate_series(1, 100) g, ld_in WHERE name = 'newer'
         ) s",
        &[],
    );
    assert!(most <= 1, "{most} expanded values alive at once");
    // Many rows stored through the cast: nothing left behind.
    Spi::run(
        "CREATE TEMP TABLE many AS SELECT g, b::automerge AS doc \
         FROM generate_series(1, 200) g, ld_in WHERE name = 'newer'",
    )
    .unwrap();
    Spi::run(
        "INSERT INTO many SELECT g, b FROM generate_series(201, 400) g, ld_in WHERE name = 'newer'",
    )
    .unwrap();
    let n: i64 = one("SELECT count(DISTINCT doc::bytea) FROM many WHERE g <= 400", &[]);
    assert_eq!(n, 1);
    assert_eq!(expanded_contexts(), 0);
    assert_eq!(am::loaded::live_count(), live_before);
}

#[pg_test]
fn verify_writes_setting_switches_the_save_and_load_check() {
    loads_fixture();
    let setting: String = one("SELECT current_setting('pg_automerge.verify_writes')", &[]);
    assert_eq!(setting, "on");
    let context: String = one(
        "SELECT context FROM pg_settings WHERE name = 'pg_automerge.verify_writes'",
        &[],
    );
    assert_eq!(context, "superuser");
    // Only superusers (or roles granted SET on it) may turn it off.
    Spi::run("CREATE ROLE ld_plain").unwrap();
    assert_eq!(
        sql_error(
            "DO $$ BEGIN SET LOCAL ROLE ld_plain; \
             SET LOCAL pg_automerge.verify_writes = off; END $$"
        ),
        "42501: permission denied to set parameter \"pg_automerge.verify_writes\""
    );

    let incremental = "UPDATE ld SET doc = merge(doc, (SELECT b FROM ld_in WHERE name = 'changes'))";
    let _failing = ReloadCheckFails::on();
    // On: merged changes are checked when stored (the hook fails it).
    assert!(sql_error(incremental).starts_with("22P02: "));
    // Off: no check, one load (the stored document).
    Spi::run("SET pg_automerge.verify_writes = off").unwrap();
    let checks = am::test_hooks::reload_checks();
    assert_eq!(loads_of(incremental), 1);
    let status: String = one("SELECT doc->>'status' FROM ld", &[]);
    assert_eq!(status, "edited");
    reset_ld();
    // Input that is not its own canonical encoding: not checked either.
    let mut doc = sample();
    doc.commit();
    let heads = doc.get_heads();
    let save = doc.save();
    doc.put(ROOT, "later", true).unwrap();
    doc.commit();
    let trailing = [save.as_slice(), &doc.save_after(&heads)].concat();
    let later: bool = one::<JsonB>("SELECT $1::automerge::jsonb", &[trailing.clone().into()]).0["later"]
        .as_bool()
        .unwrap();
    assert!(later);
    assert_eq!(am::test_hooks::reload_checks(), checks);
    // The risk it takes: an input whose re-save does not load (found by
    // the fuzz harness) is stored, and fails when it is next loaded.
    let real = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/crates/pg_automerge_core/tests/corpus/reload-b-84714.bin"
    ));
    Spi::run_with_args(
        "INSERT INTO ld VALUES (9, $1::automerge)",
        &[real.to_vec().into()],
    )
    .unwrap();
    assert!(sql_error("SELECT doc::jsonb FROM ld WHERE id = 9").starts_with("XX000: "));
    Spi::run("RESET pg_automerge.verify_writes").unwrap();
    // Back on: the same non-canonical input is checked (the hook fails it).
    assert!(
        sql_error(&format!(
            "SELECT '{}'::automerge",
            am::encoding::to_hex_literal(&trailing)
        ))
        .starts_with("22P02: invalid automerge document: does not survive a save and load (forced")
    );
    assert!(
        sql_error(&format!(
            "SELECT '{}'::automerge",
            am::encoding::to_hex_literal(real)
        ))
        .starts_with("22P02: ")
    );
}

#[pg_test]
fn no_op_merges_keep_the_stored_toast_value() {
    // About 20 kB of text that does not compress below the TOAST threshold.
    let mut doc = AutoCommit::new().with_actor(actor(1));
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let body: String = (0..20_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(b'a' + (state % 26) as u8)
        })
        .collect();
    doc.splice_text(&text, 0, 0, &body).unwrap();
    doc.commit();
    let older = am::normalize(&doc.save()).unwrap();
    let older_heads = doc.get_heads();
    doc.put(ROOT, "status", "stored").unwrap();
    doc.commit();
    let changes = doc.save_after(&older_heads);
    let save = doc.save();
    let base = am::normalize(&save).unwrap();
    Spi::run("CREATE TEMP TABLE tv (id int PRIMARY KEY, doc automerge NOT NULL)").unwrap();
    Spi::run_with_args("INSERT INTO tv VALUES (1, $1::automerge)", &[base.clone().into()]).unwrap();
    let chunk_id = || -> String { one("SELECT pg_column_toast_chunk_id(doc)::text FROM tv", &[]) };
    let stored_id = chunk_id();

    // Re-sent changes, a re-sent save, the same document and an older one:
    // the stored value comes back as it arrived (a TOAST pointer), so the
    // UPDATE keeps the TOAST value instead of writing a new one. Only the
    // older document needs loads (its cast, and the stored document to see
    // that it has the older heads).
    for (sql, arg, want_loads) in [
        ("UPDATE tv SET doc = merge(doc, $1::bytea)", changes, 0),
        ("UPDATE tv SET doc = merge(doc, $1::bytea)", save, 0),
        ("UPDATE tv SET doc = merge(doc, $1::bytea::automerge)", base.clone(), 1),
        ("UPDATE tv SET doc = merge(doc, $1::bytea::automerge)", older.clone(), 2),
        ("UPDATE tv SET doc = merge($1::bytea::automerge, doc)", older, 2),
    ] {
        let before = loads();
        Spi::run_with_args(sql, &[arg.into()]).unwrap();
        assert_eq!(loads() - before, want_loads, "{sql}");
        assert_eq!(chunk_id(), stored_id, "{sql}");
    }
    let same: bool = one("SELECT doc::bytea = $1 FROM tv", &[base.into()]);
    assert!(same);
    // A real change writes a new value.
    doc.put(ROOT, "status", "changed").unwrap();
    let newer = doc.save();
    Spi::run_with_args("UPDATE tv SET doc = merge(doc, $1::bytea)", &[newer.into()]).unwrap();
    assert_ne!(chunk_id(), stored_id);
}
