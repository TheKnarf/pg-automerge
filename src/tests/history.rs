// The history functions: automerge_changes*, automerge_get_change,
// automerge_change_count and automerge_to_jsonb(automerge, text[]).

/// base -> (a: two changes, one with message and time) and
/// (b: one change) concurrently, merged. Returns (merged, base, a, b).
fn history_docs() -> (AutoCommit, AutoCommit, AutoCommit, AutoCommit) {
    use pg_automerge_core::automerge::transaction::CommitOptions;
    let mut base = AutoCommit::new().with_actor(actor(1));
    base.put(ROOT, "title", "base").unwrap();
    base.commit_with(
        CommitOptions::default()
            .with_message("create")
            .with_time(1_700_000_000),
    );
    let mut a = base.fork().with_actor(actor(2));
    a.put(ROOT, "title", "from a").unwrap();
    a.commit();
    a.put(ROOT, "a", 1i64).unwrap();
    a.commit_with(CommitOptions::default().with_message("second a"));
    let mut b = base.fork().with_actor(actor(3));
    b.put(ROOT, "b", true).unwrap();
    b.commit();
    let mut merged = a.fork().with_actor(actor(4));
    merged.merge(&mut b.fork()).unwrap();
    (merged, base, a, b)
}

fn heads_of(doc: &mut AutoCommit) -> Vec<String> {
    let mut h: Vec<String> = doc.get_heads().iter().map(ToString::to_string).collect();
    h.sort();
    h
}

#[pg_test]
fn changes_rows_order_and_columns() {
    let (mut merged, mut base, _, _) = history_docs();
    Spi::run("CREATE TEMP TABLE hist (id int, doc automerge)").unwrap();
    Spi::run_with_args("INSERT INTO hist VALUES (1, $1)", &[merged.save().into()]).unwrap();
    let n: i64 = one("SELECT count(*) FROM hist, automerge_changes(doc)", &[]);
    assert_eq!(n, 4);
    // Causal order: every dep of a row appears on an earlier row.
    let bad: i64 = one(
        "WITH c AS (SELECT x.* FROM hist, automerge_changes(doc) WITH ORDINALITY x(hash, actor, seq, start_op, op_count, time, message, deps, change, o)) \
         SELECT count(*) FROM c, unnest(c.deps) d \
         WHERE NOT EXISTS (SELECT 1 FROM c e WHERE e.hash = d AND e.o < c.o)",
        &[],
    );
    assert_eq!(bad, 0);
    // First row: the base change, with its time and message.
    let (hash, actor_hex) = Spi::get_two::<String, String>(
        "SELECT hash, actor FROM hist, automerge_changes(doc) LIMIT 1",
    )
    .unwrap();
    assert_eq!(hash.unwrap(), heads_of(&mut base)[0]);
    assert_eq!(actor_hex.unwrap(), "01".repeat(16));
    let first: bool = one(
        "SELECT seq = 1 AND start_op = 1 AND op_count = 1 \
                AND time = to_timestamp(1700000000) AND message = 'create' \
                AND deps = '{}' AND length(change) > 0 \
         FROM hist, automerge_changes(doc) LIMIT 1",
        &[],
    );
    assert!(first);
    // Unset time and message are NULL.
    let nulls: i64 = one(
        "SELECT count(*) FROM hist, automerge_changes(doc) WHERE time IS NULL",
        &[],
    );
    assert_eq!(nulls, 3);
    let messages: Vec<String> = one(
        "SELECT array_agg(message ORDER BY message) FROM hist, automerge_changes(doc) WHERE message IS NOT NULL",
        &[],
    );
    assert_eq!(messages, vec!["create", "second a"]);
    // Metadata-only rows equal the full rows without the bytes.
    let same: bool = one(
        "SELECT array_agg(ROW(c.hash, c.actor, c.seq, c.start_op, c.op_count, c.time, c.message, c.deps)::automerge_change_meta) \
              = (SELECT array_agg(m) FROM hist, automerge_changes_meta(doc) m) \
         FROM hist, automerge_changes(doc) c",
        &[],
    );
    assert!(same);
    // Each row's bytes are exactly that change: loading them all (in
    // order) rebuilds the document.
    let rebuilt: bool = one(
        "SELECT automerge_heads(string_agg(change, ''::bytea)::automerge) \
              = (SELECT automerge_heads(doc) FROM hist) \
         FROM hist, automerge_changes(doc)",
        &[],
    );
    assert!(rebuilt);
    let concat: bool = one(
        "SELECT string_agg(change, ''::bytea) = (SELECT automerge_changes_bytes(doc) FROM hist) \
         FROM hist, automerge_changes(doc)",
        &[],
    );
    assert!(concat);
    // Column types of the result.
    let types: String = one(
        "SELECT string_agg(format_type(atttypid, atttypmod), ',' ORDER BY attnum) \
         FROM pg_attribute WHERE attrelid = 'automerge_change'::regclass AND attnum > 0",
        &[],
    );
    assert_eq!(
        types,
        "text,text,bigint,bigint,bigint,timestamp with time zone,text,text[],bytea"
    );
}

#[pg_test]
fn changes_since_heads() {
    let (mut merged, mut base, mut a, mut b) = history_docs();
    let args = [
        merged.save().into(),
        heads_of(&mut base).into(),
        heads_of(&mut a).into(),
        heads_of(&mut b).into(),
        heads_of(&mut merged).into(),
    ];
    let q = |expr: &str| {
        format!(
            "WITH v(doc, base, a, b, cur) AS (SELECT $1::automerge, $2::text[], $3::text[], $4::text[], $5::text[]) SELECT {expr} FROM v"
        )
    };
    let count = |since: &str| -> i64 {
        one(
            &q(&format!(
                "(SELECT count(*) FROM automerge_changes_meta(doc, {since}))"
            )),
            &args,
        )
    };
    assert_eq!(count("'{}'"), 4);
    assert_eq!(count("base"), 3);
    assert_eq!(count("a"), 1);
    assert_eq!(count("b"), 2);
    assert_eq!(count("a || b"), 0);
    assert_eq!(count("cur"), 0);
    // Unknown hashes are ignored (a replica ahead of the stored doc).
    assert_eq!(count("base || repeat('ab', 32)"), 3);
    assert_eq!(count("ARRAY[repeat('ab', 32)]"), 4);
    // Uppercase is accepted.
    assert_eq!(count("ARRAY[upper(a[1])]"), 1);
    let since_b: String = one(
        &q("(SELECT string_agg(hash, ',') FROM automerge_changes(doc, b))"),
        &args,
    );
    let a_changes: String = one(
        &q(
            "(SELECT string_agg(hash, ',') FROM automerge_changes(doc) c WHERE c.actor = repeat('02', 16))",
        ),
        &args,
    );
    assert_eq!(since_b, a_changes);
    let empty: Vec<u8> = one(&q("automerge_changes_bytes(doc, cur)"), &args);
    assert!(empty.is_empty());
    // Bad hashes: 22P02; NULL elements: 22004.
    for since in [
        "ARRAY['abc']",
        "ARRAY[repeat('g', 64)]",
        "ARRAY[repeat('ab', 33)]",
    ] {
        let err = sql_error(&format!(
            "SELECT count(*) FROM automerge_changes(''::bytea::automerge, {since})"
        ));
        assert!(
            err.starts_with("22P02: invalid automerge change hash"),
            "{err}"
        );
    }
    let err =
        sql_error("SELECT automerge_changes_bytes(''::bytea::automerge, ARRAY[NULL]::text[])");
    assert_eq!(err, "22004: since_heads must not contain NULL");
    // STRICT: NULL arguments give NULL / no rows.
    let null = Spi::get_one::<Vec<u8>>("SELECT automerge_changes_bytes(NULL, '{}')");
    assert_eq!(null.unwrap(), None);
    let rows: i64 = one("SELECT count(*) FROM automerge_changes(NULL)", &[]);
    assert_eq!(rows, 0);
}

#[pg_test]
fn changes_bytes_round_trip_through_merge() {
    let (mut merged, mut base, mut a, _) = history_docs();
    Spi::run("CREATE TEMP TABLE rt (id text PRIMARY KEY, doc automerge NOT NULL)").unwrap();
    Spi::run_with_args(
        "INSERT INTO rt VALUES ('full', $1), ('base', $2), ('a', $3)",
        &[merged.save().into(), base.save().into(), a.save().into()],
    )
    .unwrap();
    for replica in ["base", "a"] {
        let ok: bool = one(
            "SELECT automerge_heads(m) = automerge_heads(f.doc) AND m::jsonb = f.doc::jsonb \
             FROM rt f, rt r, \
                  LATERAL (SELECT merge(r.doc, automerge_changes_bytes(f.doc, automerge_heads(r.doc)))) x(m) \
             WHERE f.id = 'full' AND r.id = $1",
            &[replica.into()],
        );
        assert!(ok, "{replica}");
    }
    // The delta is only what the replica lacks.
    let n: i64 = one(
        "SELECT count(*) FROM rt f, rt r, automerge_changes(f.doc, automerge_heads(r.doc)) \
         WHERE f.id = 'full' AND r.id = 'a'",
        &[],
    );
    assert_eq!(n, 1);
    // In an UPDATE: bring a stored replica up to date.
    Spi::run(
        "UPDATE rt r SET doc = merge(r.doc, automerge_changes_bytes(f.doc, automerge_heads(r.doc))) \
         FROM rt f WHERE f.id = 'full' AND r.id = 'base'",
    )
    .unwrap();
    let heads: Vec<String> = one("SELECT automerge_heads(doc) FROM rt WHERE id = 'base'", &[]);
    assert_eq!(heads, heads_of(&mut merged));
    // Loadable by Automerge itself, on top of the replica.
    let delta: Vec<u8> = one(
        "SELECT automerge_changes_bytes(f.doc, automerge_heads(r.doc)) FROM rt f, rt r \
         WHERE f.id = 'full' AND r.id = 'a'",
        &[],
    );
    let mut replica = a.fork();
    replica.load_incremental(&delta).unwrap();
    assert_eq!(heads_of(&mut replica), heads_of(&mut merged));
}

#[pg_test]
fn get_change_by_hash() {
    let (mut merged, mut base, _, _) = history_docs();
    let base_head = heads_of(&mut base)[0].clone();
    let args = [merged.save().into(), base_head.clone().into()];
    let msg: String = one(
        "SELECT (automerge_get_change($1::automerge, $2)).message",
        &args,
    );
    assert_eq!(msg, "create");
    let same: bool = one(
        "SELECT automerge_get_change($1::automerge, upper($2)) \
              = (SELECT c FROM automerge_changes($1::automerge) c WHERE c.hash = $2)",
        &args,
    );
    assert!(same);
    let change: Vec<u8> = one(
        "SELECT (automerge_get_change($1::automerge, $2)).change",
        &args,
    );
    let loaded = pg_automerge_core::automerge::Change::from_bytes(change).unwrap();
    assert_eq!(loaded.hash().to_string(), base_head);
    let missing: bool = one(
        "SELECT automerge_get_change($1::automerge, repeat('00', 32)) IS NULL",
        &args[..1],
    );
    assert!(missing);
    let err = sql_error(&format!(
        "SELECT automerge_get_change('{}'::automerge, 'nope')",
        pg_automerge_core::encoding::to_hex_literal(&merged.save())
    ));
    assert_eq!(
        err,
        "22P02: invalid automerge change hash \"nope\": expected 64 hexadecimal digits"
    );
}

#[pg_test]
fn to_jsonb_at_heads() {
    let (mut merged, mut base, mut a, mut b) = history_docs();
    let args = [
        merged.save().into(),
        heads_of(&mut base).into(),
        heads_of(&mut a).into(),
        heads_of(&mut b).into(),
    ];
    let q = |heads: &str| -> JsonB {
        one(
            &format!(
                "SELECT automerge_to_jsonb($1::automerge, {heads}) FROM (SELECT $2::text[], $3::text[], $4::text[]) v(base, a, b)"
            ),
            &args,
        )
    };
    assert_eq!(q("base").0, json!({"title": "base"}));
    assert_eq!(q("a").0, json!({"title": "from a", "a": 1}));
    assert_eq!(q("b").0, json!({"title": "base", "b": true}));
    assert_eq!(q("a || b").0, json!({"title": "from a", "a": 1, "b": true}));
    assert_eq!(q("'{}'").0, json!({}));
    // The current heads give the current state.
    let current: bool = one(
        "SELECT automerge_to_jsonb($1::automerge, automerge_heads($1::automerge)) = $1::automerge::jsonb",
        &args[..1],
    );
    assert!(current);
    // Every change's state, via the change list.
    let n: i64 = one(
        "SELECT count(DISTINCT automerge_to_jsonb($1::automerge, ARRAY[hash])) FROM automerge_changes_meta($1::automerge)",
        &args[..1],
    );
    assert_eq!(n, 4);
    let hex = pg_automerge_core::encoding::to_hex_literal(&merged.save());
    let unknown = "cd".repeat(32);
    let err = sql_error(&format!(
        "SELECT automerge_to_jsonb('{hex}'::automerge, ARRAY['{unknown}'])"
    ));
    assert_eq!(
        err,
        format!("22023: automerge document does not contain change {unknown}")
    );
    let err = sql_error(&format!(
        "SELECT automerge_to_jsonb('{hex}'::automerge, ARRAY['x'])"
    ));
    assert!(err.starts_with("22P02: "), "{err}");
    let err = sql_error(&format!(
        "SELECT automerge_to_jsonb('{hex}'::automerge, '{{NULL}}'::text[])"
    ));
    assert_eq!(err, "22004: heads must not contain NULL");
    // The one-argument cast function is unaffected.
    let cast: bool = one(
        "SELECT automerge_to_jsonb($1::automerge) = $1::automerge::jsonb",
        &args[..1],
    );
    assert!(cast);
}

#[pg_test]
fn change_count_for_every_storage_form() {
    Spi::run("CREATE TEMP TABLE cc (id int, doc automerge NOT NULL)").unwrap();
    let (mut merged, _, _, _) = history_docs();
    let mut many = AutoCommit::new();
    for i in 0..300u16 {
        many.set_actor(actor((i % 7) as u8 + 1));
        many.put(ROOT, format!("k{i}"), i64::from(i)).unwrap();
        many.commit();
    }
    let mut big = AutoCommit::new().with_actor(actor(9));
    big.put(ROOT, "pad", "abcdefgh".repeat(50_000)).unwrap();
    big.commit();
    big.put(ROOT, "more", 1i64).unwrap();
    for (i, bytes) in [
        merged.save(),
        many.save(),
        big.save(),
        AutoCommit::new().save(),
        large_doc().save(),
    ]
    .into_iter()
    .enumerate()
    {
        Spi::run_with_args(
            "INSERT INTO cc VALUES ($1, $2)",
            &[(i as i32).into(), bytes.into()],
        )
        .unwrap();
    }
    let counts: Vec<i64> = one(
        "SELECT array_agg(automerge_change_count(doc) ORDER BY id) FROM cc",
        &[],
    );
    // large_doc() is one change: AutoCommit commits once, on save.
    assert_eq!(counts, vec![4, 300, 2, 0, 1]);
    let disagree: i64 = one(
        "SELECT count(*) FROM cc \
         WHERE automerge_change_count(doc) <> (SELECT count(*) FROM automerge_changes_meta(doc))",
        &[],
    );
    assert_eq!(disagree, 0);
}

/// SRFs stopped early (LIMIT on a target-list SRF, closed cursors) and
/// SRFs failing mid-statement leave nothing behind.
#[pg_test]
fn changes_srf_early_termination() {
    let (mut merged, _, _, _) = history_docs();
    Spi::run("CREATE TEMP TABLE et (doc automerge)").unwrap();
    Spi::run_with_args("INSERT INTO et VALUES ($1)", &[merged.save().into()]).unwrap();
    for _ in 0..50 {
        let h: String = one("SELECT (automerge_changes(doc)).hash FROM et LIMIT 1", &[]);
        assert_eq!(h.len(), 64);
        let h: String = one(
            "SELECT (automerge_changes_meta(doc, '{}')).hash FROM et LIMIT 1",
            &[],
        );
        assert_eq!(h.len(), 64);
    }
    Spi::run(
        "DO $$ DECLARE c refcursor; r record; BEGIN \
           FOR i IN 1..20 LOOP \
             OPEN c FOR SELECT (automerge_changes(doc)).* FROM et; \
             FETCH c INTO r; \
             CLOSE c; \
           END LOOP; END $$",
    )
    .unwrap();
    // An error from a later argument after earlier rows were produced.
    let err =
        sql_error("SELECT (automerge_changes(doc)).hash, 1 / (random() * 0)::int FROM et");
    assert!(err.starts_with("22012: "), "{err}");
    let n: i64 = one("SELECT count(*) FROM et, automerge_changes(doc)", &[]);
    assert_eq!(n, 4);
}

#[pg_test]
fn history_functions_are_labelled_and_search_path_safe() {
    let wrong: Vec<String> = one(
        "SELECT coalesce(array_agg(p.oid::regprocedure::text), '{}') FROM pg_proc p \
         WHERE p.proname IN ('automerge_changes', 'automerge_changes_meta', 'automerge_changes_bytes', \
                             'automerge_get_change', 'automerge_change_count', 'automerge_to_jsonb') \
           AND NOT (p.provolatile = 'i' AND p.proisstrict AND p.proparallel = 's')",
        &[],
    );
    assert!(wrong.is_empty(), "{wrong:?}");
    let n: i64 = one(
        "SELECT count(*) FROM pg_proc WHERE proname IN ('automerge_changes', 'automerge_changes_meta', \
         'automerge_changes_bytes', 'automerge_get_change', 'automerge_change_count', 'automerge_to_jsonb')",
        &[],
    );
    assert_eq!(n, 7);
    // Result rows are built from the function's declared type, not a
    // lookup by name, so they work with the extension off search_path.
    let (mut merged, _, _, _) = history_docs();
    let schema: String = one(
        "SELECT n.nspname::text FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
         WHERE t.typname = 'automerge_change'",
        &[],
    );
    Spi::run("SET LOCAL search_path TO pg_catalog").unwrap();
    let n: i64 = one(
        &format!(
            "SELECT count(*) FROM {schema}.automerge_changes($1::bytea::{schema}.automerge) c \
             WHERE (c).change IS NOT NULL"
        ),
        &[merged.save().into()],
    );
    Spi::run("RESET search_path").unwrap();
    assert_eq!(n, 4);
}

/// The error of running `alter` then `sql` in a subtransaction that is
/// rolled back either way ("P0001: no error" if `sql` succeeds).
fn error_after_alter(alter: &str, sql: &str, doc: &[u8]) -> String {
    Spi::run(
        "CREATE OR REPLACE FUNCTION pg_temp.error_after_alter(a text, q text, d bytea) \
         RETURNS text LANGUAGE plpgsql AS $$ \
         BEGIN EXECUTE a; EXECUTE q USING d; RAISE EXCEPTION 'no error'; \
         EXCEPTION WHEN OTHERS THEN RETURN SQLSTATE || ': ' || SQLERRM; END $$",
    )
    .unwrap();
    one(
        "SELECT pg_temp.error_after_alter($1, $2, $3)",
        &[alter.into(), sql.into(), doc.to_vec().into()],
    )
}

// The owner of the change types can alter them; the rows used to be built
// from the altered descriptor anyway, so an int8 was read as a text pointer
// and the backend crashed (SIGSEGV). Now every altered shape is XX000.
#[pg_test]
fn changes_reject_altered_result_types() {
    let (mut merged, mut base, _, _) = history_docs();
    let doc = merged.save();
    let head = heads_of(&mut base)[0].clone();
    let changes = "SELECT count(*) FROM automerge_changes($1::automerge)";
    let meta = "SELECT count(*) FROM automerge_changes_meta($1::automerge)";
    let get = format!("SELECT automerge_get_change($1::automerge, '{head}') IS NULL");
    let full = "type automerge_change has been altered: its attributes must be those \
                created by extension pg_automerge";
    let short = "type automerge_change_meta has been altered: its attributes must be those \
                 created by extension pg_automerge";
    let cases: [(&str, &str, &str); 7] = [
        ("ALTER TYPE automerge_change ALTER ATTRIBUTE seq TYPE text", changes, full),
        ("ALTER TYPE automerge_change ALTER ATTRIBUTE seq TYPE text", &get, full),
        ("ALTER TYPE automerge_change ALTER ATTRIBUTE change TYPE text", changes, full),
        ("ALTER TYPE automerge_change_meta ALTER ATTRIBUTE start_op TYPE numeric", meta, short),
        (
            "CREATE DOMAIN pg_temp.t AS text; \
             ALTER TYPE automerge_change_meta ALTER ATTRIBUTE hash TYPE pg_temp.t",
            meta,
            short,
        ),
        (
            "ALTER TYPE automerge_change_meta DROP ATTRIBUTE deps, ADD ATTRIBUTE deps text[]",
            meta,
            short,
        ),
        ("ALTER TYPE automerge_change DROP ATTRIBUTE change", changes, full),
    ];
    for (alter, sql, message) in cases {
        let err = error_after_alter(alter, sql, &doc);
        assert_eq!(err, format!("XX000: {message}"), "{alter}; {sql}");
    }
    // Unaltered (the subtransactions rolled back): rows as before.
    for sql in [changes, meta] {
        let err = error_after_alter("SELECT 1", sql, &doc);
        assert_eq!(err, "P0001: no error", "{sql}");
        let n: i64 = one(sql, &[doc.clone().into()]);
        assert_eq!(n, 4);
    }
    let found: bool = one(&get, &[doc.clone().into()]);
    assert!(!found);
}
