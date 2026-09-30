// Memory observability: automerge_memory_usage() follows loads, in-memory
// (expanded) values, merge_agg states and set-returning functions, and
// returns to where it was after them, after errors in the middle of an
// operation and over many statements. tests/memory.sh does the same across
// top-level statements, transactions and a real statement_timeout.

/// One row of `automerge_memory_usage()`.
#[derive(Debug, Clone, Copy)]
struct Usage {
    allocated: i64,
    peak: i64,
    live: i64,
    loads: i64,
    load_time: f64,
}

fn usage() -> Usage {
    Spi::connect(|client| {
        let row = client
            .select(
                "SELECT allocated_bytes, peak_allocated_bytes, live_documents, loads, load_time \
                 FROM automerge_memory_usage()",
                None,
                &[],
            )
            .unwrap()
            .first();
        Usage {
            allocated: row.get(1).unwrap().unwrap(),
            peak: row.get(2).unwrap().unwrap(),
            live: row.get(3).unwrap().unwrap(),
            loads: row.get(4).unwrap().unwrap(),
            load_time: row.get(5).unwrap().unwrap(),
        }
    })
}

/// Slack for Rust allocations of the test itself and of pgrx around a
/// statement (SPI strings, cached lookups): far below any document here.
const SLACK: i64 = 64 * 1024;

/// A document of about `n` random characters (random so that pglz keeps
/// it large and its load takes megabytes), as a compressed save.
fn noise_doc(n: usize, seed: u64) -> Vec<u8> {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    let mut x: u64 = seed | 1;
    let noise: String = (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            char::from(b'!' + (x % 90) as u8)
        })
        .collect();
    doc.put(ROOT, "noise", noise).unwrap();
    doc.save()
}

/// Table `mt`: row 1 a document of 200,000 characters, row 2 it with a
/// concurrent change by another actor (the change chunk is table `mc`'s
/// one row), row 3 a document of the first actor with the same seq 1 as
/// row 1 but other content (merging it with row 1 is a 22000 conflict),
/// row 4 a document of 2,000 changes, row 5 a document nested deeper than
/// the jsonb view allows (54000), row 6 a copy of row 1 (to update).
/// Returns row 1's save.
fn memory_fixture() -> Vec<u8> {
    let base = noise_doc(200_000, 7);
    let mut other = AutoCommit::load(&base).unwrap().with_actor(actor(2));
    other.put(ROOT, "other", 1).unwrap();
    let changes = other.save_after(&AutoCommit::load(&base).unwrap().get_heads());
    let conflict = noise_doc(10, 9);
    let many = {
        let mut doc = AutoCommit::new().with_actor(actor(3));
        for i in 0..2000 {
            doc.put(ROOT, "n", i64::from(i)).unwrap();
            doc.commit();
        }
        doc.save()
    };
    let deep = {
        let mut doc = AutoCommit::new();
        let mut obj = ROOT;
        for _ in 0..am::json::MAX_DEPTH {
            obj = doc.put_object(&obj, "k", ObjType::Map).unwrap();
        }
        doc.save()
    };
    Spi::run("CREATE TEMP TABLE mt (k int PRIMARY KEY, doc automerge)").unwrap();
    Spi::run_with_args(
        "CREATE TEMP TABLE mc AS SELECT $1::bytea AS c",
        &[changes.clone().into()],
    )
    .unwrap();
    Spi::run_with_args(
        "INSERT INTO mt VALUES (1, $1::bytea::automerge), (2, merge($1::bytea::automerge, $2)), \
         (3, $3::bytea::automerge), (4, $4::bytea::automerge), (5, $5::bytea::automerge), \
         (6, $1::bytea::automerge)",
        &[
            base.clone().into(),
            changes.into(),
            conflict.into(),
            many.into(),
            deep.into(),
        ],
    )
    .unwrap();
    base
}

#[pg_test]
fn memory_usage_is_one_row_and_resets() {
    let n: i64 = one("SELECT count(*) FROM automerge_memory_usage()", &[]);
    assert_eq!(n, 1);
    let labels: String = one(
        "SELECT string_agg(proname || ' ' || provolatile::text || proparallel::text, ', ' ORDER BY proname) \
         FROM pg_proc WHERE proname IN ('automerge_memory_usage', 'automerge_memory_reset')",
        &[],
    );
    assert_eq!(labels, "automerge_memory_reset vr, automerge_memory_usage vr");
    let u = usage();
    assert!(u.allocated > 0 && u.peak >= u.allocated, "{u:?}");
    Spi::run("SELECT automerge_memory_reset()").unwrap();
    let r = usage();
    assert_eq!((r.loads, r.load_time), (0, 0.0), "{r:?}");
    // The peak starts over from the allocation at the reset.
    assert!(
        r.peak >= r.allocated && r.peak < r.allocated + SLACK,
        "{u:?} {r:?}"
    );
}

#[pg_test]
fn memory_usage_follows_loads_and_values() {
    let base = memory_fixture();
    let _ = usage(); // warm up SPI and pgrx's lookups
    let start = usage();

    // A read loads the document: counted, and gone again afterwards; the
    // peak shows what it took.
    Spi::run("SELECT automerge_memory_reset()").unwrap();
    let len: i32 = one("SELECT length(doc->>'noise') FROM mt WHERE k = 1", &[]);
    assert_eq!(len, 200_000);
    let after = usage();
    assert_eq!(after.loads, 1, "{after:?}");
    assert!(after.load_time > 0.0);
    assert!(
        after.peak > start.allocated + base.len() as i64,
        "{start:?} {after:?}"
    );
    assert!(after.allocated < start.allocated + SLACK, "{start:?} {after:?}");
    assert_eq!(after.live, start.live);

    // An in-memory value holds its document while it lives.
    Spi::run(
        "CREATE FUNCTION pg_temp.held() RETURNS bigint[] LANGUAGE plpgsql AS $$
         DECLARE d automerge; ch bytea; u0 record; u1 record;
         BEGIN
             SELECT * INTO u0 FROM automerge_memory_usage();
             SELECT doc INTO d FROM mt WHERE k = 1;
             SELECT c INTO ch FROM mc;
             -- A simple expression (a subquery would make PL/pgSQL run it
             -- through SPI, which flattens the result).
             d := merge(d, ch);
             SELECT * INTO u1 FROM automerge_memory_usage();
             RETURN ARRAY[u1.live_documents - u0.live_documents,
                          u1.allocated_bytes - u0.allocated_bytes];
         END $$",
    )
    .unwrap();
    let held: Vec<i64> = one("SELECT pg_temp.held()", &[]);
    assert_eq!(held[0], 1, "{held:?}");
    assert!(held[1] > base.len() as i64, "{held:?}");
    let after = usage();
    assert_eq!(after.live, start.live);
    assert!(after.allocated < start.allocated + SLACK, "{start:?} {after:?}");

    // merge_agg of two unrelated documents: its state holds a document
    // while the aggregate runs (seen from a window frame: at the second
    // row the state has loaded one, and the frame's result is another),
    // and nothing after.
    // (In the target list, and with `m` used: a function scan in a
    // subquery that does not depend on the row is run once and rescanned,
    // and an unused window result is not computed.)
    let live: Vec<i64> = one(
        "SELECT array_agg(n ORDER BY k) FILTER (WHERE used) FROM ( \
             SELECT k, m IS NOT NULL AS used, (automerge_memory_usage()).live_documents AS n \
             FROM (SELECT k, merge_agg(doc) OVER (ORDER BY k) AS m FROM mt WHERE k IN (1, 4)) s \
         ) t",
        &[],
    );
    assert!(live[1] >= start.live + 2, "{start:?} {live:?}");
    Spi::run("SELECT automerge_memory_reset()").unwrap();
    let heads: i64 = one(
        "SELECT cardinality(automerge_heads(merge_agg(doc))) FROM mt WHERE k IN (1, 4)",
        &[],
    );
    assert_eq!(heads, 2);
    let after = usage();
    assert!(after.peak > start.allocated + base.len() as i64, "{after:?}");
    assert_eq!(after.live, start.live);
    assert!(after.allocated < start.allocated + SLACK, "{start:?} {after:?}");

    // A set-returning function keeps its rows until it is done: called
    // from a target list (value per call; in FROM, Postgres collects every
    // row into a tuplestore at once), a cursor that fetched one row holds
    // them, closing it frees them, and a LIMIT that stops early frees
    // them too.
    let start = usage();
    Spi::run("DECLARE cur CURSOR FOR SELECT automerge_changes(doc) FROM mt WHERE k = 4").unwrap();
    Spi::run("FETCH 1 FROM cur").unwrap();
    let open = usage();
    assert!(
        open.allocated > start.allocated + 2000 * 64,
        "{start:?} {open:?}: the SRF's rows"
    );
    Spi::run("CLOSE cur").unwrap();
    let closed = usage();
    assert!(closed.allocated < start.allocated + SLACK, "{start:?} {closed:?}");
    let n: i64 = one(
        "SELECT count(*) FROM (SELECT automerge_changes(doc) FROM mt WHERE k = 4 LIMIT 3) s",
        &[],
    );
    assert_eq!(n, 3);
    let after = usage();
    assert!(after.allocated < start.allocated + SLACK, "{start:?} {after:?}");
    assert_eq!(after.live, start.live);
}

#[pg_test]
fn memory_usage_recovers_from_errors() {
    memory_fixture();
    let _ = usage();
    let start = usage();
    // Each fails part way: inside a merge after both inputs are loaded,
    // part way through a jsonb walk, with a merge_agg state loaded, while
    // a set-returning function holds its rows, on input, and when an
    // in-memory value is flattened (the save-and-load check, forced to
    // fail).
    let failing = [
        (
            "SELECT merge(a.doc, b.doc) FROM mt a, mt b WHERE a.k = 1 AND b.k = 3",
            "22000",
        ),
        ("SELECT doc::jsonb FROM mt WHERE k = 5", "54000"),
        (
            "SELECT merge_agg(doc ORDER BY k) FROM mt WHERE k IN (1, 2, 3)",
            "22000",
        ),
        (
            "SELECT sum(CASE WHEN seq > 1000 THEN 1 / 0 END) \
             FROM automerge_changes((SELECT doc FROM mt WHERE k = 4))",
            "22012",
        ),
        ("SELECT '\\x856f4a83ffff'::bytea::automerge", "22P02"),
    ];
    for _ in 0..3 {
        for (sql, code) in failing {
            let err = sql_error(sql);
            assert!(err.starts_with(code), "{sql}: {err}");
        }
        {
            let _fails = ReloadCheckFails::on();
            let err = sql_error("UPDATE mt SET doc = merge(doc, (SELECT c FROM mc)) WHERE k = 1");
            assert!(err.starts_with("22P02"), "{err}");
        }
        let after = usage();
        assert_eq!(after.live, start.live, "{start:?} {after:?}");
        assert!(after.allocated < start.allocated + SLACK, "{start:?} {after:?}");
    }
}

#[pg_test]
fn memory_usage_does_not_leak_over_many_statements() {
    memory_fixture();
    Spi::run(
        "CREATE FUNCTION pg_temp.round() RETURNS void LANGUAGE plpgsql AS $$
         DECLARE d automerge; x automerge; ch bytea; j jsonb; n bigint;
         BEGIN
             SELECT doc INTO d FROM mt WHERE k = 1;
             SELECT doc INTO x FROM mt WHERE k = 4;
             SELECT c INTO ch FROM mc;
             d := merge(d, ch);
             d := d || x;
             j := d::jsonb;
             SELECT count(*) INTO n FROM automerge_changes_meta(d);
             SELECT count(*) INTO n FROM (SELECT merge_agg(doc) FROM mt WHERE k IN (1, 4)) s;
             UPDATE mt SET doc = (SELECT doc FROM mt WHERE k = 1) WHERE k = 6;
             UPDATE mt SET doc = merge(doc, (SELECT c FROM mc)) WHERE k = 6;
             SELECT count(*) INTO n FROM (SELECT hash FROM automerge_changes(
                 (SELECT doc FROM mt WHERE k = 4)) LIMIT 2) s;
             BEGIN
                 PERFORM merge(a.doc, b.doc) FROM mt a, mt b WHERE a.k = 1 AND b.k = 3;
             EXCEPTION WHEN data_exception THEN NULL;
             END;
             BEGIN
                 PERFORM '\\x856f4a83ffff'::bytea::automerge;
             EXCEPTION WHEN invalid_text_representation THEN NULL;
             END;
         END $$",
    )
    .unwrap();
    // Warm up: pgrx and Postgres cache lookups on first use.
    for _ in 0..5 {
        Spi::run("SELECT pg_temp.round()").unwrap();
    }
    let start = usage();
    Spi::run("SELECT automerge_memory_reset()").unwrap();
    for _ in 0..200 {
        Spi::run("SELECT pg_temp.round()").unwrap();
    }
    let after = usage();
    assert!(after.loads >= 200, "{after:?}");
    // Measured: no drift at all; 8 kB catches a leak of 41 bytes a round.
    assert_eq!(after.live, start.live, "{start:?} {after:?}");
    assert!(
        after.allocated < start.allocated + 8192,
        "{start:?} {after:?}: leaked {} bytes over 200 rounds",
        after.allocated - start.allocated
    );
}
