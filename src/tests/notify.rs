// The automerge_notify() trigger.

/// Notifications sent so far in this backend (channel, parsed payload
/// without its `seq`), clearing the list. Checks that `seq` increases
/// strictly across all notifications of the backend.
fn take_sent() -> Vec<(String, serde_json::Value)> {
    thread_local! {
        static LAST_SEQ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    crate::notify::SENT_NOTIFICATIONS.with(|s| {
        s.borrow_mut()
            .drain(..)
            .map(|(channel, payload)| {
                assert!(payload.len() < 8000, "{}", payload.len());
                assert!(
                    !payload.contains(": ") && !payload.contains(", "),
                    "not compact: {payload}"
                );
                let mut v: serde_json::Value = serde_json::from_str(&payload).unwrap();
                let seq = v
                    .as_object_mut()
                    .unwrap()
                    .remove("seq")
                    .and_then(|s| s.as_u64())
                    .unwrap_or_else(|| panic!("no seq in {payload}"));
                assert!(seq > LAST_SEQ.get(), "seq {seq} after {}", LAST_SEQ.get());
                LAST_SEQ.set(seq);
                (channel, v)
            })
            .collect()
    })
}

#[pg_test]
fn notify_trigger_repeated_identical_events_are_distinct() {
    // NOTIFY drops a notification whose channel and payload equal an
    // earlier one of the same transaction: INSERT, DELETE, INSERT of
    // the same row, or a key changing 1 -> 2 -> 1 -> 2, must still give
    // distinct payloads (they differ in seq).
    Spi::run(
        "CREATE TABLE nr (id int PRIMARY KEY, doc automerge); \
         CREATE TRIGGER nr_notify AFTER INSERT OR UPDATE OR DELETE ON nr \
           FOR EACH ROW EXECUTE FUNCTION automerge_notify('nr', 'id')",
    )
    .unwrap();
    take_sent();
    Spi::run(
        "INSERT INTO nr VALUES (1, '\\x'::bytea); DELETE FROM nr WHERE id = 1; \
         INSERT INTO nr VALUES (1, '\\x'::bytea); \
         UPDATE nr SET id = 2; UPDATE nr SET id = 1; UPDATE nr SET id = 2",
    )
    .unwrap();
    let raw: Vec<String> =
        crate::notify::SENT_NOTIFICATIONS.with(|s| s.borrow().iter().map(|(_, p)| p.clone()).collect());
    assert_eq!(raw.len(), 6, "{raw:?}");
    let distinct: std::collections::HashSet<_> = raw.iter().collect();
    assert_eq!(distinct.len(), 6, "{raw:?}");
    let ops: Vec<String> = take_sent()
        .into_iter()
        .map(|(_, v)| {
            format!(
                "{}{}{}",
                v["op"],
                v["key"],
                v.get("old_key").map_or(String::new(), |k| k.to_string())
            )
        })
        .collect();
    assert_eq!(
        ops,
        [
            r#""INSERT"{"id":1}"#,
            r#""DELETE"{"id":1}"#,
            r#""INSERT"{"id":1}"#,
            r#""UPDATE"{"id":2}{"id":1}"#,
            r#""UPDATE"{"id":1}{"id":2}"#,
            r#""UPDATE"{"id":2}{"id":1}"#,
        ]
    );
}

fn heads_json(sql: &str) -> serde_json::Value {
    one::<JsonB>(&format!("SELECT to_jsonb(automerge_heads(({sql})))"), &[]).0
}

#[pg_test]
fn notify_trigger_payloads() {
    Spi::run(
        "CREATE TABLE nt (id int, tenant text, doc automerge, other automerge, note text, \
                          PRIMARY KEY (id, tenant)); \
         CREATE TRIGGER nt_notify AFTER INSERT OR UPDATE OR DELETE ON nt \
           FOR EACH ROW EXECUTE FUNCTION automerge_notify('nt_changes', 'id', 'tenant')",
    )
    .unwrap();
    take_sent();
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "n", 0i64).unwrap();
    Spi::run_with_args(
        "INSERT INTO nt VALUES (1, 'a\"b', $1, NULL, 'x')",
        &[doc.save().into()],
    )
    .unwrap();
    doc.save_incremental();
    let row = "SELECT doc FROM nt WHERE id = 1";
    let v0 = heads_json(row);
    assert_eq!(v0.as_array().unwrap().len(), 1);
    assert_eq!(
        take_sent(),
        vec![(
            "nt_changes".to_string(),
            json!({
                "table": "public.nt",
                "op": "INSERT",
                "key": {"id": 1, "tenant": "a\"b"},
                "columns": {"doc": {"heads": v0}, "other": {"heads": null}},
            })
        )]
    );

    // New changes: heads and prev_heads of the changed column only.
    doc.put(ROOT, "n", 1i64).unwrap();
    let change = doc.save_incremental();
    Spi::run_with_args(
        "UPDATE nt SET doc = merge(doc, $1)",
        &[change.clone().into()],
    )
    .unwrap();
    let v1 = heads_json(row);
    assert_ne!(v0, v1);
    assert_eq!(
        take_sent(),
        vec![(
            "nt_changes".to_string(),
            json!({
                "table": "public.nt",
                "op": "UPDATE",
                "key": {"id": 1, "tenant": "a\"b"},
                "columns": {"doc": {"heads": v1, "prev_heads": v0}},
            })
        )]
    );

    // No-op merges (same change again, full save, the row's own value)
    // and updates of other columns do not notify.
    Spi::run_with_args("UPDATE nt SET doc = merge(doc, $1)", &[change.into()]).unwrap();
    Spi::run_with_args("UPDATE nt SET doc = merge(doc, $1)", &[doc.save().into()]).unwrap();
    Spi::run("UPDATE nt SET doc = merge(doc, doc), note = 'y'").unwrap();
    Spi::run("UPDATE nt SET doc = doc::bytea").unwrap();
    // Key columns set to equal values (raw-equal, or equal as JSON).
    Spi::run("UPDATE nt SET id = id, tenant = tenant").unwrap();
    Spi::run("UPDATE nt SET tenant = tenant || '', id = id + 0").unwrap();
    assert_eq!(take_sent(), vec![]);

    // A NULL column becoming a document, and a key change.
    Spi::run("UPDATE nt SET other = ''::bytea, id = 2").unwrap();
    assert_eq!(
        take_sent()[0].1,
        json!({
            "table": "public.nt",
            "op": "UPDATE",
            "key": {"id": 2, "tenant": "a\"b"},
            "old_key": {"id": 1, "tenant": "a\"b"},
            "columns": {"other": {"heads": [], "prev_heads": null}},
        })
    );
    // Only the key changes: still notified (listeners track rows by key).
    Spi::run("UPDATE nt SET id = 3").unwrap();
    let sent = take_sent();
    assert_eq!(sent[0].1["old_key"], json!({"id": 2, "tenant": "a\"b"}));
    assert_eq!(sent[0].1["columns"], json!({}));

    Spi::run("DELETE FROM nt").unwrap();
    assert_eq!(
        take_sent()[0].1,
        json!({
            "table": "public.nt",
            "op": "DELETE",
            "key": {"id": 3, "tenant": "a\"b"},
            "columns": {"doc": {"prev_heads": v1}, "other": {"prev_heads": []}},
        })
    );

    // Works with the extension's schema off search_path, and for a
    // quoted table name.
    Spi::run(
        "CREATE TABLE \"Odd Name\" (k uuid PRIMARY KEY, d automerge); \
         CREATE TRIGGER t AFTER INSERT ON \"Odd Name\" \
           FOR EACH ROW EXECUTE FUNCTION automerge_notify('odd', 'k')",
    )
    .unwrap();
    Spi::run("SET LOCAL search_path TO pg_catalog").unwrap();
    Spi::run(
        "INSERT INTO public.\"Odd Name\" \
         VALUES ('00000000-0000-0000-0000-000000000001', '\\x'::bytea)",
    )
    .unwrap();
    Spi::run("RESET search_path").unwrap();
    assert_eq!(
        take_sent(),
        vec![(
            "odd".to_string(),
            json!({
                "table": "public.\"Odd Name\"",
                "op": "INSERT",
                "key": {"k": "00000000-0000-0000-0000-000000000001"},
                "columns": {"d": {"heads": []}},
            })
        )]
    );
}

#[pg_test]
fn notify_trigger_on_toasted_and_many_head_documents() {
    Spi::run(
        "CREATE TABLE big_n (id int PRIMARY KEY, doc automerge, note text); \
         CREATE TRIGGER big_notify AFTER INSERT OR UPDATE OR DELETE ON big_n \
           FOR EACH ROW EXECUTE FUNCTION automerge_notify('big', 'id')",
    )
    .unwrap();
    let mut large = large_doc();
    Spi::run_with_args(
        "INSERT INTO big_n VALUES (1, $1, 'x')",
        &[large.save().into()],
    )
    .unwrap();
    let toast_bytes: i64 = one(
        "SELECT pg_relation_size(reltoastrelid) FROM pg_class WHERE oid = 'big_n'::regclass",
        &[],
    );
    assert!(toast_bytes > 1_000_000, "toast size {toast_bytes}");
    let heads = heads_json("SELECT doc FROM big_n");
    let sent = take_sent();
    assert_eq!(sent[0].1["columns"]["doc"]["heads"], heads);
    // Another column changes: same TOAST pointer, no notification.
    Spi::run("UPDATE big_n SET note = 'y'").unwrap();
    assert_eq!(take_sent(), vec![]);
    large.put(ROOT, "more", 1i64).unwrap();
    Spi::run_with_args(
        "UPDATE big_n SET doc = merge(doc, $1)",
        &[large.save().into()],
    )
    .unwrap();
    let sent = take_sent();
    assert_eq!(sent[0].1["columns"]["doc"]["prev_heads"], heads);
    let heads = heads_json("SELECT doc FROM big_n");
    assert_eq!(sent[0].1["columns"]["doc"]["heads"], heads);
    Spi::run("DELETE FROM big_n").unwrap();
    assert_eq!(take_sent()[0].1["columns"]["doc"]["prev_heads"], heads);

    // 150 heads do not fit into a payload: heads are dropped, the write
    // succeeds, the rest stays.
    let mut base = AutoCommit::new();
    let mut many = AutoCommit::new().with_actor(actor(200));
    for i in 0..150u8 {
        let mut fork = base.fork().with_actor(ActorId::from([i; 16]));
        fork.put(ROOT, format!("k{i}"), 1i64).unwrap();
        many.merge(&mut fork).unwrap();
    }
    Spi::run_with_args("INSERT INTO big_n VALUES (2, $1)", &[many.save().into()]).unwrap();
    let n: i64 = one("SELECT cardinality(automerge_heads(doc)) FROM big_n", &[]);
    assert_eq!(n, 150);
    assert_eq!(
        take_sent()[0].1,
        json!({
            "table": "public.big_n",
            "op": "INSERT",
            "key": {"id": 2},
            "columns": {"doc": {}},
            "truncated": true,
        })
    );
}

#[pg_test]
fn notify_trigger_validates_usage() {
    Spi::run("CREATE TABLE nv (id int PRIMARY KEY, doc automerge)").unwrap();
    let cases = [
        (
            "BEFORE INSERT ON nv FOR EACH ROW EXECUTE FUNCTION automerge_notify('c', 'id')",
            "39P01: automerge_notify() must be fired AFTER, not BEFORE",
        ),
        (
            "AFTER INSERT ON nv FOR EACH STATEMENT EXECUTE FUNCTION automerge_notify('c', 'id')",
            "39P01: automerge_notify() must be fired FOR EACH ROW",
        ),
        (
            "AFTER INSERT ON nv FOR EACH ROW EXECUTE FUNCTION automerge_notify('c')",
            "22023: automerge_notify() needs a channel and at least one key column",
        ),
        (
            "AFTER INSERT ON nv FOR EACH ROW EXECUTE FUNCTION automerge_notify('c', 'nope')",
            "42703: automerge_notify(): key column \"nope\" does not exist in table public.nv",
        ),
        (
            "AFTER INSERT ON nv FOR EACH ROW EXECUTE FUNCTION automerge_notify('c', 'id', 'id')",
            "22023: automerge_notify(): key column \"id\" is listed twice",
        ),
        (
            "AFTER INSERT ON nv FOR EACH ROW EXECUTE FUNCTION automerge_notify('c', 'doc')",
            "22023: automerge_notify(): key column \"doc\" is an automerge column",
        ),
        (
            "AFTER INSERT ON nv FOR EACH ROW EXECUTE FUNCTION automerge_notify('', 'id')",
            "22023: automerge_notify(): channel name must be 1 to 63 bytes, got 0",
        ),
    ];
    for (definition, expected) in cases {
        Spi::run("DROP TRIGGER IF EXISTS t ON nv").unwrap();
        Spi::run(&format!("CREATE TRIGGER t {definition}")).unwrap();
        let err = sql_error("INSERT INTO nv VALUES (1, NULL)");
        assert!(err.starts_with(expected), "{definition}: {err}");
    }
    let long = "x".repeat(64);
    Spi::run("DROP TRIGGER IF EXISTS t ON nv").unwrap();
    Spi::run(&format!(
        "CREATE TRIGGER t AFTER INSERT ON nv FOR EACH ROW EXECUTE FUNCTION automerge_notify('{long}', 'id')"
    ))
    .unwrap();
    let err = sql_error("INSERT INTO nv VALUES (1, NULL)");
    assert!(
        err.starts_with(
            "22023: automerge_notify(): channel name must be 1 to 63 bytes, got 64"
        ),
        "{err}"
    );
    // A failed trigger fails the write: nothing was inserted.
    let n: i64 = one("SELECT count(*) FROM nv", &[]);
    assert_eq!(n, 0);
    take_sent();

    // Labels: an ordinary volatile, non-strict, parallel-unsafe trigger
    // function (it sends notifications).
    let labels: String = one(
        "SELECT provolatile::text || proisstrict::text || proparallel::text || prorettype::regtype::text \
         FROM pg_proc WHERE proname = 'automerge_notify'",
        &[],
    );
    assert_eq!(labels, "vfalseutrigger");
    // The usage goes in the HINT, the message stays short.
    let err = sql_error_report("SELECT automerge_notify()");
    assert_eq!(err[..2], ["39P01", "automerge_notify() can only be called as a trigger"]);
    assert_eq!(err[2], "");
    assert!(err[3].starts_with("Declare it as CREATE TRIGGER ... AFTER"), "{err:?}");
    Spi::run("DROP TRIGGER IF EXISTS t ON nv").unwrap();
    Spi::run("CREATE TRIGGER t BEFORE INSERT ON nv FOR EACH ROW EXECUTE FUNCTION automerge_notify('c', 'id')").unwrap();
    let err = sql_error_report("INSERT INTO nv VALUES (1, NULL)");
    assert_eq!(
        err[..3],
        [
            "39P01",
            "automerge_notify() must be fired AFTER, not BEFORE (trigger \"t\")",
            ""
        ]
    );
    assert!(err[3].contains("automerge_notify('channel', 'key_column' [, ...])"), "{err:?}");
    // An automerge column as the key: the advice is in the HINT.
    Spi::run("DROP TRIGGER IF EXISTS t ON nv").unwrap();
    Spi::run("CREATE TRIGGER t AFTER INSERT ON nv FOR EACH ROW EXECUTE FUNCTION automerge_notify('c', 'doc')").unwrap();
    let err = sql_error_report("INSERT INTO nv VALUES (1, NULL)");
    assert_eq!(
        err[..3],
        [
            "22023",
            "automerge_notify(): key column \"doc\" is an automerge column (trigger \"t\")",
            ""
        ]
    );
    assert!(err[3].starts_with("Name the columns that identify the row"), "{err:?}");
}
