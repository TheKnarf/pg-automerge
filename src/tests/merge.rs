// merge (both overloads), ||, merge_agg, automerge_heads and
// automerge_contains (both overloads), and the heads fast path.

#[pg_test]
fn merge_commutative_and_idempotent() {
    let mut base = AutoCommit::new().with_actor(actor(1));
    base.put(ROOT, "title", "base").unwrap();
    let list = base.put_object(ROOT, "list", ObjType::List).unwrap();
    base.insert(&list, 0, "x").unwrap();
    let mut a = base.fork().with_actor(actor(2));
    let mut b = base.fork().with_actor(actor(3));
    a.put(ROOT, "title", "from a").unwrap();
    a.insert(&list, 1, "a").unwrap();
    b.put(ROOT, "b", 1i64).unwrap();
    b.insert(&list, 1, "b").unwrap();

    let args = [a.save().into(), b.save().into(), base.save().into()];
    let q = |expr: &str| {
        format!(
            "WITH v(a, b, base) AS (SELECT $1::automerge, $2::automerge, $3::automerge) SELECT {expr} FROM v"
        )
    };

    // Unqualified merge(...) works despite MERGE being a keyword.
    let same_heads: bool = one(
        &q("automerge_heads(merge(a, b)) = automerge_heads(merge(b, a))"),
        &args,
    );
    assert!(same_heads);
    let same_json: bool = one(&q("merge(a, b)::jsonb = merge(b, a)::jsonb"), &args);
    assert!(same_json);
    let op: bool = one(&q("(a || b)::jsonb = merge(a, b)::jsonb"), &args);
    assert!(op);
    let n_heads: i32 = one(&q("cardinality(automerge_heads(merge(a, b)))"), &args);
    assert_eq!(n_heads, 2);

    let json: JsonB = one(&q("merge(a, b)::jsonb"), &args);
    a.merge(&mut b).unwrap();
    assert_eq!(json.0["title"], "from a");
    assert_eq!(json.0["b"], 1);
    // Same state as merging in Rust.
    let merged = pg_automerge_core::normalize(&a.save()).unwrap();
    assert_eq!(json.0, stored_json(&merged));
    assert_eq!(json.0["list"].as_array().unwrap().len(), 3);

    // Idempotent, and merging an ancestor is a byte-for-byte no-op.
    let idem: bool = one(
        &q("merge(merge(a, b), b)::bytea = merge(a, b)::bytea"),
        &args,
    );
    assert!(idem);
    let self_merge: bool = one(&q("merge(a, a)::bytea = a::bytea"), &args);
    assert!(self_merge);
    let ancestor: bool = one(&q("merge(a, base)::bytea = a::bytea"), &args);
    assert!(ancestor);
    let ancestor_left: bool = one(&q("merge(base, a)::bytea = a::bytea"), &args);
    assert!(ancestor_left);
}

#[pg_test]
fn merge_in_update_statement() {
    let mut base = AutoCommit::new().with_actor(actor(1));
    base.put(ROOT, "n", 0i64).unwrap();
    let mut a = base.fork().with_actor(actor(2));
    let mut b = base.fork().with_actor(actor(3));
    a.put(ROOT, "a", "yes").unwrap();
    b.put(ROOT, "b", "yes").unwrap();
    Spi::run("CREATE TEMP TABLE upd (id int PRIMARY KEY, doc automerge NOT NULL)").unwrap();
    Spi::run_with_args("INSERT INTO upd VALUES (1, $1)", &[base.save().into()]).unwrap();
    Spi::run_with_args(
        "UPDATE upd SET doc = merge(doc, $1::automerge) WHERE id = 1",
        &[a.save().into()],
    )
    .unwrap();
    Spi::run_with_args(
        "INSERT INTO upd VALUES (1, $1) ON CONFLICT (id) DO UPDATE SET doc = merge(upd.doc, EXCLUDED.doc)",
        &[b.save().into()],
    )
    .unwrap();
    let json: JsonB = one("SELECT doc::jsonb FROM upd", &[]);
    assert_eq!(json.0, json!({ "n": 0, "a": "yes", "b": "yes" }));
}

#[pg_test]
fn merge_agg_merges_all_rows() {
    let mut base = AutoCommit::new().with_actor(actor(1));
    base.put(ROOT, "base", true).unwrap();
    Spi::run("CREATE TEMP TABLE agg (doc automerge)").unwrap();
    Spi::run_with_args("INSERT INTO agg VALUES ($1), (NULL)", &[base.save().into()]).unwrap();
    for i in 2..6u8 {
        let mut fork = base.fork().with_actor(actor(i));
        fork.put(ROOT, format!("k{i}"), i64::from(i)).unwrap();
        Spi::run_with_args("INSERT INTO agg VALUES ($1)", &[fork.save().into()]).unwrap();
    }
    let json: JsonB = one("SELECT merge_agg(doc)::jsonb FROM agg", &[]);
    assert_eq!(
        json.0,
        json!({ "base": true, "k2": 2, "k3": 3, "k4": 4, "k5": 5 })
    );
    let heads: i32 = one(
        "SELECT cardinality(automerge_heads(merge_agg(doc))) FROM agg",
        &[],
    );
    assert_eq!(heads, 4);
    // Order-independent.
    let same: bool = one(
        "SELECT (SELECT merge_agg(doc ORDER BY doc::text) FROM agg)::bytea IS NOT NULL \
           AND (SELECT automerge_heads(merge_agg(doc ORDER BY doc::text DESC)) FROM agg) \
             = (SELECT automerge_heads(merge_agg(doc ORDER BY doc::text)) FROM agg)",
        &[],
    );
    assert!(same);
    // All-NULL / empty input gives NULL.
    let none =
        Spi::get_one::<Vec<u8>>("SELECT merge_agg(doc)::bytea FROM agg WHERE doc IS NULL");
    assert_eq!(none.unwrap(), None);
    // Grouped use.
    let groups: i64 = one(
        "SELECT count(*) FROM (SELECT merge_agg(doc) FROM agg GROUP BY doc IS NULL) s",
        &[],
    );
    assert_eq!(groups, 2);
}

#[pg_test]
fn merge_agg_as_window_function() {
    Spi::run("CREATE TEMP TABLE win (id int, doc automerge)").unwrap();
    let mut base = AutoCommit::new().with_actor(actor(1));
    for i in 1..=4u8 {
        let mut fork = base.fork().with_actor(actor(i + 1));
        fork.put(ROOT, format!("k{i}"), i64::from(i)).unwrap();
        Spi::run_with_args(
            "INSERT INTO win VALUES ($1, $2)",
            &[i32::from(i).into(), fork.save().into()],
        )
        .unwrap();
    }
    // Running merge: the final function is called repeatedly on a state
    // that keeps growing.
    let keys: Vec<i64> = Spi::connect(|client| {
        client
            .select(
                "SELECT (SELECT count(*) FROM jsonb_object_keys(m)) FROM \
                   (SELECT id, merge_agg(doc) OVER (ORDER BY id)::jsonb FROM win) s(id, m) \
                 ORDER BY id",
                None,
                &[],
            )
            .unwrap()
            .map(|row| row.get::<i64>(1).unwrap().unwrap())
            .collect()
    });
    assert_eq!(keys, vec![1, 2, 3, 4]);
    // Sliding frame: Postgres restarts the aggregate (resetting its
    // memory context) for every row, since there is no inverse function.
    let sliding: Vec<JsonB> = Spi::connect(|client| {
        client
            .select(
                "SELECT merge_agg(doc) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)::jsonb \
                 FROM win ORDER BY id",
                None,
                &[],
            )
            .unwrap()
            .map(|row| row.get::<JsonB>(1).unwrap().unwrap())
            .collect()
    });
    let sliding: Vec<_> = sliding.into_iter().map(|j| j.0).collect();
    assert_eq!(
        sliding,
        vec![
            json!({ "k1": 1 }),
            json!({ "k1": 1, "k2": 2 }),
            json!({ "k2": 2, "k3": 3 }),
            json!({ "k3": 3, "k4": 4 }),
        ]
    );
}

#[pg_test]
fn merge_agg_declares_realistic_state_size() {
    // The Rust-heap state is not spillable; a realistic aggtransspace
    // keeps the planner from assuming ~8kB per HashAgg group.
    let space: i32 = one(
        "SELECT aggtransspace FROM pg_aggregate WHERE aggfnoid = 'merge_agg'::regproc",
        &[],
    );
    assert_eq!(space, 1_048_576);
}

#[pg_test]
fn reused_actor_id_is_a_data_exception_with_a_hint() {
    // Two different histories that both claim (actor 1, seq 1): a
    // backend bug, e.g. a hard-coded actor id. Automerge refuses to
    // merge them; every way of combining them raises the same 22000
    // (data_exception) error naming the actor and seq, with a HINT, and
    // never a crash or an XX000.
    let mut base = AutoCommit::new().with_actor(actor(2));
    base.put(ROOT, "base", true).unwrap();
    base.commit();
    let base_heads = base.get_heads();
    let mut a = base.fork().with_actor(actor(1));
    let mut b = base.fork().with_actor(actor(1));
    a.put(ROOT, "x", 1i64).unwrap();
    b.put(ROOT, "x", 2i64).unwrap();
    let hex = |bytes: &[u8]| pg_automerge_core::encoding::to_hex_literal(bytes);
    let (ha, hb) = (hex(&a.save()), hex(&b.save()));
    let hb_change = hex(&b.save_after(&base_heads));
    Spi::run(&format!(
        "CREATE TABLE dup (id int PRIMARY KEY, doc automerge); \
         INSERT INTO dup VALUES (1, '{ha}'), (2, '{hb}')"
    ))
    .unwrap();
    let expected_message = format!(
        "conflicting automerge changes: actor {} has two different changes with seq 1",
        actor(1)
    );
    for sql in [
        format!("SELECT merge('{ha}'::bytea::automerge, '{hb}'::bytea::automerge)"),
        "SELECT merge(x.doc, y.doc) FROM dup x, dup y WHERE x.id = 1 AND y.id = 2".into(),
        format!("SELECT '{ha}'::bytea::automerge || '{hb}'::bytea::automerge"),
        "SELECT merge_agg(doc) FROM dup".into(),
        // merge(automerge, bytea): a full save and a bare change chunk.
        format!("UPDATE dup SET doc = merge(doc, '{hb}'::bytea) WHERE id = 1"),
        format!("UPDATE dup SET doc = doc || '{hb_change}'::bytea WHERE id = 1"),
        format!("SELECT automerge_contains(doc, '{hb_change}'::bytea) FROM dup WHERE id = 1"),
        // Concatenated saves go through Automerge::load (the bytea cast and
        // text input).
        format!("SELECT ('{ha}'::bytea || '{hb}'::bytea)::automerge"),
        format!("SELECT '{ha}{}'::automerge", &hb[2..]),
        // In place, in PL/pgSQL.
        format!(
            "DO $$ DECLARE d automerge; BEGIN \
               SELECT doc INTO d FROM dup WHERE id = 1; \
               d := merge(d, '{hb_change}'::bytea); \
             END $$"
        ),
    ] {
        let [code, message, detail, hint]: [String; 4] =
            sql_error_report(&sql).try_into().unwrap();
        assert_eq!(code, "22000", "{sql}: {message}");
        assert_eq!(message, expected_message, "{sql}");
        assert!(detail.ends_with("which cannot be merged."), "{sql}: {detail}");
        assert!(hint.starts_with("Each writer must use its own actor id."), "{sql}: {hint}");
    }
    // The failed writes changed nothing.
    let x: JsonB = one("SELECT doc::jsonb FROM dup WHERE id = 1", &[]);
    assert_eq!(x.0, json!({ "base": true, "x": 1 }));
}

#[pg_test]
fn merge_unrelated_documents_in_sql() {
    let mut a = AutoCommit::new().with_actor(actor(1));
    let mut b = AutoCommit::new().with_actor(actor(2));
    a.put(ROOT, "same", "a").unwrap();
    a.put(ROOT, "only_a", 1i64).unwrap();
    b.put(ROOT, "same", "b").unwrap();
    b.put(ROOT, "only_b", 2i64).unwrap();
    let args = [a.save().into(), b.save().into()];
    let json: JsonB = one("SELECT merge($1::automerge, $2::automerge)::jsonb", &args);
    assert_eq!(json.0, json!({ "same": "b", "only_a": 1, "only_b": 2 }));
    let symmetric: bool = one(
        "SELECT automerge_heads($1::automerge || $2::automerge) \
              = automerge_heads($2::automerge || $1::automerge)",
        &args,
    );
    assert!(symmetric);
}

/// The extension functions and operators a one-column view over `expr`
/// depends on, as `regprocedure` / `regoperator` text, sorted, followed
/// by `=> <result type>`. This is what the parser resolved `expr` to.
/// (Built-in objects are pinned and have no pg_depend entries, so e.g.
/// jsonb's `||` shows up only through the result type.)
fn resolved(expr: &str) -> Vec<String> {
    Spi::run("DROP VIEW IF EXISTS resolve_v").unwrap();
    Spi::run(&format!(
        "CREATE TEMP VIEW resolve_v AS SELECT {expr} AS x FROM resolve_t"
    ))
    .unwrap();
    let mut deps: Vec<String> = one(
        "SELECT coalesce(array_agg(x ORDER BY x), '{}') FROM ( \
           SELECT coalesce(p.oid::regprocedure::text, o.oid::regoperator::text) AS x \
         FROM pg_depend d JOIN pg_rewrite r ON d.classid = 'pg_rewrite'::regclass AND d.objid = r.oid \
         LEFT JOIN pg_proc p ON d.refclassid = 'pg_proc'::regclass AND p.oid = d.refobjid \
         LEFT JOIN pg_operator o ON d.refclassid = 'pg_operator'::regclass AND o.oid = d.refobjid \
         WHERE r.ev_class = 'resolve_v'::regclass AND (p.oid IS NOT NULL OR o.oid IS NOT NULL)) s",
        &[],
    );
    let typ: String = one(
        "SELECT atttypid::regtype::text FROM pg_attribute \
         WHERE attrelid = 'resolve_v'::regclass AND attname = 'x'",
        &[],
    );
    deps.push(format!("=> {typ}"));
    deps
}

#[pg_test]
fn merge_overloads_resolve_unambiguously() {
    Spi::run("CREATE TEMP TABLE resolve_t (d automerge, b bytea)").unwrap();
    let mm = "merge(automerge,automerge)";
    let mb = "merge(automerge,bytea)";
    let op_mm = "||(automerge,automerge)";
    let op_mb = "||(automerge,bytea)";
    let am = "=> automerge";
    for (expr, expected) in [
        ("d || d", vec![op_mm, am]),
        ("d || b", vec![op_mb, am]),
        ("d || '\\x'::bytea", vec![op_mb, am]),
        // An untyped literal takes the other operand's type.
        ("d || '\\x'", vec![op_mm, am]),
        // jsonb || jsonb (built in, so only the cast is listed).
        (
            "d::jsonb || '{\"a\": 1}'",
            vec!["automerge_to_jsonb(automerge)", "=> jsonb"],
        ),
        ("merge(d, d)", vec![mm, am]),
        ("merge(d, b)", vec![mb, am]),
        ("merge(d, '\\x'::bytea)", vec![mb, am]),
        // Unknown literal / NULL: both candidates accept it, and it is
        // assumed to have the known argument's type (automerge).
        ("merge(d, '\\x')", vec![mm, am]),
        ("merge(d, NULL)", vec![mm, am]),
    ] {
        assert_eq!(resolved(expr), expected, "{expr}");
    }
    // A typed bytea parameter (what drivers send) picks the bytea
    // overload without a cast; an untyped one is inferred as automerge.
    Spi::run("PREPARE typed_p(bytea) AS SELECT merge(d, $1), d || $1 FROM resolve_t").unwrap();
    Spi::run("PREPARE untyped_p AS SELECT merge(d, $1) FROM resolve_t").unwrap();
    let types: String = one(
        "SELECT string_agg(name || '=' || parameter_types::text, ' ' ORDER BY name) \
         FROM pg_prepared_statements WHERE name IN ('typed_p', 'untyped_p')",
        &[],
    );
    assert_eq!(types, "typed_p={bytea} untyped_p={automerge}");
    let plan = explain_with("VERBOSE, COSTS OFF", "EXECUTE typed_p('\\x')");
    assert!(plan.contains("merge(d, '\\x'::bytea)"), "{plan}");
    assert!(plan.contains("(d || '\\x'::bytea)"), "{plan}");
    Spi::run("DEALLOCATE typed_p; DEALLOCATE untyped_p").unwrap();
    // Labels are truthful and match merge(automerge, automerge).
    let labels: String = one(
        "SELECT string_agg(p.oid::regprocedure || ':' || provolatile::text || proisstrict::text || proparallel::text, ' ' ORDER BY p.oid::regprocedure::text) \
         FROM pg_proc p WHERE proname = 'merge'",
        &[],
    );
    assert_eq!(
        labels,
        "merge(automerge,automerge):itrues merge(automerge,bytea):itrues"
    );
}

#[pg_test]
fn merge_bytea_persists_incremental_changes() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "n", 0i64).unwrap();
    Spi::run("CREATE TEMP TABLE inc (id int PRIMARY KEY, doc automerge NOT NULL)").unwrap();
    Spi::run_with_args("INSERT INTO inc VALUES (1, $1)", &[doc.save().into()]).unwrap();
    doc.save_incremental();

    // One chunk per update, bound as a typed bytea parameter (Vec<u8>),
    // no cast in the SQL.
    let update = "UPDATE inc SET doc = merge(doc, $1) WHERE id = 1";
    for i in 1..=3i64 {
        doc.put(ROOT, "n", i).unwrap();
        doc.put(ROOT, format!("k{i}"), i).unwrap();
        Spi::run_with_args(update, &[doc.save_incremental().into()]).unwrap();
    }
    // Several chunks concatenated, through a PREPAREd statement with a
    // bytea parameter, and through the operator.
    Spi::run("PREPARE persist(bytea) AS UPDATE inc SET doc = merge(doc, $1) WHERE id = 1")
        .unwrap();
    let mut chunks = Vec::new();
    for i in 4..=5i64 {
        doc.put(ROOT, "n", i).unwrap();
        doc.commit();
        chunks.extend(doc.save_incremental());
    }
    let hex = pg_automerge_core::encoding::to_hex_literal(&chunks);
    Spi::run(&format!("EXECUTE persist('{hex}')")).unwrap();
    Spi::run("DEALLOCATE persist").unwrap();
    doc.put(ROOT, "via", "operator").unwrap();
    Spi::run_with_args(
        "UPDATE inc SET doc = doc || $1 WHERE id = 1",
        &[doc.save_incremental().into()],
    )
    .unwrap();

    // The stored value is exactly the normalized full document.
    let stored_bytes: Vec<u8> = one("SELECT doc::bytea FROM inc", &[]);
    assert_eq!(stored_bytes, doc.document().save_nocompress());
    let json: JsonB = one("SELECT doc::jsonb FROM inc", &[]);
    assert_eq!(
        json.0,
        json!({ "n": 5, "k1": 1, "k2": 2, "k3": 3, "via": "operator" })
    );

    // No-ops return the input bytes: empty, already-applied chunks, the
    // full save, NULL is NULL.
    let full = doc.save();
    let noop: bool = one(
        "SELECT merge(doc, ''::bytea)::bytea = doc::bytea \
            AND merge(doc, $1)::bytea = doc::bytea \
            AND (doc || $1)::bytea = doc::bytea \
            AND merge(doc, NULL::bytea) IS NULL FROM inc",
        &[full.into()],
    );
    assert!(noop);
    // A full save of a concurrent fork merges like merge(a, b).
    let mut fork = doc.fork().with_actor(actor(2));
    fork.put(ROOT, "fork", true).unwrap();
    let fork_save = fork.save();
    let same: bool = one(
        "SELECT automerge_heads(merge(doc, $1)) = automerge_heads(merge(doc, $1::automerge)) \
            AND merge(doc, $1)::jsonb = merge(doc, $1::automerge)::jsonb FROM inc",
        &[fork_save.into()],
    );
    assert!(same);
}

#[pg_test]
fn merge_bytea_rejects_bad_input() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "x", 1i64).unwrap();
    let base = doc.save();
    let base_hex = pg_automerge_core::encoding::to_hex_literal(&base);
    doc.put(ROOT, "y", 2i64).unwrap();
    let skipped = doc.get_heads()[0].to_string();
    doc.save_incremental();
    doc.put(ROOT, "z", 3i64).unwrap();
    let orphan = pg_automerge_core::encoding::to_hex_literal(&doc.save_incremental());

    // The hashes go in the DETAIL, the message stays short.
    let err = sql_error_report(&format!(
        "SELECT merge('{base_hex}'::bytea::automerge, '{orphan}'::bytea)"
    ));
    assert_eq!(
        err,
        [
            "22P02".to_string(),
            "invalid automerge changes: missing 1 dependency that neither the document nor the input contains".to_string(),
            format!("Missing changes: {skipped}."),
            String::new(),
        ]
    );
    for bad in ["\\x0102", "\\x856f4a83", "\\xdeadbeefdeadbeefdeadbeef"] {
        let err = sql_error(&format!(
            "SELECT merge('{base_hex}'::bytea::automerge, '{bad}'::bytea)"
        ));
        assert!(
            err.starts_with("22P02: invalid automerge changes: "),
            "{bad}: {err}"
        );
    }
    // A good chunk followed by garbage fails as a whole.
    let err = sql_error(&format!(
        "SELECT merge('\\x'::automerge, '{base_hex}'::bytea || '\\x00'::bytea)"
    ));
    assert!(err.starts_with("22P02: "), "{err}");
    // An orphan passed as a literal without ::bytea resolves to the
    // automerge overload and is rejected by the automerge input function.
    let err = sql_error(&format!(
        "SELECT merge('{base_hex}'::bytea::automerge, '{orphan}')"
    ));
    assert_eq!(
        err,
        "22P02: invalid automerge document: changes are missing dependencies"
    );
}

#[pg_test]
fn heads_and_contains() {
    let mut a = AutoCommit::new().with_actor(actor(1));
    a.put(ROOT, "x", 1i64).unwrap();
    let old = a.save();
    a.put(ROOT, "x", 2i64).unwrap();
    let expected: Vec<String> = a.get_heads().iter().map(ToString::to_string).collect();
    let heads: Vec<String> = one("SELECT automerge_heads($1::automerge)", &[a.save().into()]);
    assert_eq!(heads, expected);
    assert_eq!(heads[0].len(), 64);
    assert_eq!(heads[0], heads[0].to_lowercase());

    let args = [a.save().into(), old.into()];
    let c: bool = one(
        "SELECT automerge_contains($1::automerge, $2::automerge)",
        &args,
    );
    assert!(c);
    let c: bool = one(
        "SELECT automerge_contains($2::automerge, $1::automerge)",
        &args,
    );
    assert!(!c);
    let empty: Vec<String> = one("SELECT automerge_heads(''::bytea::automerge)", &[]);
    assert!(empty.is_empty());
}

#[pg_test]
fn heads_fast_path_matches_rust_for_every_storage_form() {
    Spi::run("CREATE TEMP TABLE hf (id int PRIMARY KEY, doc automerge NOT NULL, heads text[])")
        .unwrap();
    // Tiny (inline, short varlena header), compressible (inline
    // compressed), large compressible (external compressed), large
    // incompressible (external uncompressed), many heads, empty.
    let mut cases: Vec<AutoCommit> = Vec::new();
    // Distinct actors throughout: the pairwise merges below would fail
    // on two histories claiming the same (actor, seq).
    let mut tiny = AutoCommit::new().with_actor(actor(11));
    tiny.put(ROOT, "x", 1i64).unwrap();
    cases.push(tiny);
    for (n, len) in [(12, 6_000usize), (13, 400_000)] {
        let mut d = AutoCommit::new().with_actor(actor(n));
        d.put(ROOT, "pad", "abcdefgh".repeat(len / 8)).unwrap();
        cases.push(d);
    }
    // 150 concurrent forks of one base: 150 heads, 150 actors.
    let mut base = AutoCommit::new().with_actor(actor(14));
    base.put(ROOT, "base", true).unwrap();
    let mut wide = base.fork().with_actor(actor(15));
    for i in 0..150u8 {
        let mut id = [0xee; 16];
        id[0] = i;
        let mut f = base.fork().with_actor(ActorId::from(id));
        f.put(ROOT, format!("k{i}"), i64::from(i)).unwrap();
        wide.merge(&mut f).unwrap();
    }
    cases.push(wide);
    cases.push(AutoCommit::new());
    for (i, mut d) in cases.into_iter().enumerate() {
        let mut expected: Vec<String> = d.get_heads().iter().map(ToString::to_string).collect();
        expected.sort();
        Spi::run_with_args(
            "INSERT INTO hf VALUES ($1, $2, $3)",
            &[(i as i32).into(), d.save().into(), expected.into()],
        )
        .unwrap();
    }
    Spi::run_with_args(
        "INSERT INTO hf VALUES (100, $1, $2)",
        &[
            large_doc().save().into(),
            {
                let mut h: Vec<String> = large_doc()
                    .get_heads()
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                h.sort();
                h
            }
            .into(),
        ],
    )
    .unwrap();
    let bad: i64 = one(
        "SELECT count(*) FROM hf WHERE automerge_heads(doc) IS DISTINCT FROM heads",
        &[],
    );
    assert_eq!(bad, 0);
    // The storage forms above really occur.
    let forms: String = one(
        "SELECT string_agg(DISTINCT CASE \
             WHEN pg_column_size(doc) < octet_length(doc::bytea) THEN 'compressed' \
             ELSE 'plain' END, ',') FROM hf",
        &[],
    );
    assert_eq!(forms, "compressed,plain");
    let n_heads: i32 = one(
        "SELECT cardinality(automerge_heads(doc)) FROM hf WHERE id = 3",
        &[],
    );
    assert_eq!(n_heads, 150);
    // contains agrees with the definition (a merge would be a no-op),
    // over all pairs, using heads-only and loading decisions.
    let disagree: i64 = one(
        "SELECT count(*) FROM hf a, hf b \
         WHERE automerge_contains(a.doc, b.doc) \
               IS DISTINCT FROM (automerge_heads(merge(a.doc, b.doc)) = automerge_heads(a.doc))",
        &[],
    );
    assert_eq!(disagree, 0);
    let true_pairs: i64 = one(
        "SELECT count(*) FROM hf a, hf b WHERE automerge_contains(a.doc, b.doc)",
        &[],
    );
    // Everything contains itself (6) and the empty document (5 more).
    assert_eq!(true_pairs, 11);
}

#[pg_test]
fn contains_bytea_overload() {
    Spi::run("CREATE TEMP TABLE resolve_t (d automerge, b bytea)").unwrap();
    let cm = "automerge_contains(automerge,automerge)";
    let cb = "automerge_contains(automerge,bytea)";
    for (expr, expected) in [
        ("automerge_contains(d, d)", vec![cm, "=> boolean"]),
        ("automerge_contains(d, b)", vec![cb, "=> boolean"]),
        (
            "automerge_contains(d, '\\x'::bytea)",
            vec![cb, "=> boolean"],
        ),
        // Untyped literal / NULL: assumed to be automerge, as for merge.
        ("automerge_contains(d, '\\x')", vec![cm, "=> boolean"]),
        ("automerge_contains(d, NULL)", vec![cm, "=> boolean"]),
    ] {
        assert_eq!(resolved(expr), expected, "{expr}");
    }
    Spi::run("PREPARE typed_c(bytea) AS SELECT automerge_contains(d, $1) FROM resolve_t")
        .unwrap();
    let plan = explain_with("VERBOSE, COSTS OFF", "EXECUTE typed_c('\\x')");
    assert!(
        plan.contains("automerge_contains(d, '\\x'::bytea)"),
        "{plan}"
    );
    Spi::run("DEALLOCATE typed_c").unwrap();
    let labels: String = one(
        "SELECT string_agg(p.oid::regprocedure || ':' || provolatile::text || proisstrict::text || proparallel::text, ' ' ORDER BY p.oid::regprocedure::text) \
         FROM pg_proc p WHERE proname = 'automerge_contains'",
        &[],
    );
    assert_eq!(
        labels,
        "automerge_contains(automerge,automerge):itrues automerge_contains(automerge,bytea):itrues"
    );

    // Semantics.
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "n", 0i64).unwrap();
    let base_heads = doc.get_heads();
    Spi::run("CREATE TEMP TABLE c (id int PRIMARY KEY, doc automerge)").unwrap();
    Spi::run_with_args("INSERT INTO c VALUES (1, $1)", &[doc.save().into()]).unwrap();
    doc.save_incremental();
    doc.put(ROOT, "n", 1i64).unwrap();
    let first = doc.save_incremental();
    doc.put(ROOT, "n", 2i64).unwrap();
    let second = doc.save_incremental();
    let contains = |changes: &[u8]| -> bool {
        one(
            "SELECT automerge_contains(doc, $1) FROM c WHERE id = 1",
            &[changes.to_vec().into()],
        )
    };
    assert!(contains(&[]));
    assert!(!contains(&first));
    // `second` depends on `first`, which the row lacks: not contained.
    assert!(!contains(&second));
    assert!(!contains(&doc.save()));
    Spi::run_with_args("UPDATE c SET doc = merge(doc, $1)", &[doc.save().into()]).unwrap();
    assert!(contains(&first) && contains(&second));
    assert!(contains(&[first.clone(), second.clone()].concat()));
    assert!(contains(&doc.save()));
    assert!(contains(&doc.save_after(&base_heads)));
    let null: Option<bool> =
        Spi::get_one_with_args("SELECT automerge_contains(doc, NULL::bytea) FROM c", &[])
            .unwrap();
    assert_eq!(null, None);
    assert!(
        sql_error("SELECT automerge_contains(doc, '\\xdeadbeef'::bytea) FROM c")
            .starts_with("22P02: invalid automerge changes"),
    );
}

/// `UPDATE .. SET doc = merge(doc, $1) WHERE .. AND NOT
/// automerge_contains(doc, $1)` writes a new row version only when
/// there is something new.
#[pg_test]
fn update_only_when_not_contained() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "n", 0i64).unwrap();
    Spi::run("CREATE TEMP TABLE p (id int PRIMARY KEY, doc automerge NOT NULL)").unwrap();
    Spi::run_with_args("INSERT INTO p VALUES (1, $1)", &[doc.save().into()]).unwrap();
    doc.save_incremental();
    let update = "WITH u AS (UPDATE p SET doc = merge(doc, $1) \
                  WHERE id = 1 AND NOT automerge_contains(doc, $1) RETURNING 1) \
                  SELECT count(*) FROM u";
    let ctid = || -> String { one("SELECT ctid::text FROM p", &[]) };
    doc.put(ROOT, "n", 1i64).unwrap();
    let change = doc.save_incremental();
    let before = ctid();
    let n: i64 = one(update, &[change.clone().into()]);
    assert_eq!(n, 1);
    let after = ctid();
    assert_ne!(before, after);
    // Re-sending the same change, or the full save, updates nothing: no
    // new row version.
    for bytes in [change, doc.save(), Vec::new()] {
        let n: i64 = one(update, &[bytes.into()]);
        assert_eq!(n, 0);
        assert_eq!(ctid(), after);
    }
    let json: JsonB = one("SELECT doc::jsonb FROM p", &[]);
    assert_eq!(json.0, json!({ "n": 1 }));
}

/// A document chunk whose header lists the document's own head but whose
/// body does not parse is rejected (22P02) by `merge(automerge, bytea)` and
/// `automerge_contains(automerge, bytea)`, stored or expanded, as a load of
/// `doc ++ bytes` rejects it; one that parses is decided from its header.
#[pg_test]
fn saves_with_known_heads_and_malformed_bodies_are_rejected() {
    let mut doc = AutoCommit::new().with_actor(actor(1));
    doc.put(ROOT, "x", 1i64).unwrap();
    let older = doc.save();
    let older_heads = doc.get_heads();
    doc.put(ROOT, "y", 2i64).unwrap();
    let newer = doc.save_after(&older_heads);
    Spi::run("CREATE TEMP TABLE probe(doc automerge, older automerge, newer bytea)").unwrap();
    Spi::run_with_args(
        "INSERT INTO probe VALUES ($1::automerge, $2::automerge, $3)",
        &[doc.save().into(), older.into(), newer.into()],
    )
    .unwrap();
    // magic || sha256(0x00 || len || data)[..4] || 0x00 || len || data,
    // data = no actors, one head (the document's), then `body`.
    Spi::run(
        "CREATE FUNCTION pg_temp.known_chunk(body bytea) RETURNS bytea LANGUAGE sql AS $$ \
         WITH d(data) AS (SELECT '\\x0001'::bytea \
             || decode((SELECT automerge_heads(doc) FROM probe)[1], 'hex') || body) \
         SELECT '\\x856f4a83'::bytea \
             || substring(sha256('\\x00'::bytea || set_byte('\\x00'::bytea, 0, length(data)) || data) \
                          FROM 1 FOR 4) \
             || '\\x00'::bytea || set_byte('\\x00'::bytea, 0, length(data)) || data FROM d $$",
    )
    .unwrap();
    let garbage = "pg_temp.known_chunk('\\xffffffff0102')";
    for sql in [
        format!("SELECT merge(doc, {garbage}) FROM probe"),
        format!("SELECT doc || {garbage} FROM probe"),
        format!("SELECT automerge_contains(doc, {garbage}) FROM probe"),
        format!(
            "DO $$ DECLARE d automerge; BEGIN SELECT merge(older, newer) INTO d FROM probe; \
             d := merge(d, {garbage}); END $$"
        ),
    ] {
        let err = sql_error(&sql);
        assert!(err.starts_with("22P02: invalid automerge changes"), "{sql}: {err}");
    }
    // A known chunk that parses (no columns, one head index): a no-op.
    let empty = "pg_temp.known_chunk('\\x000000')";
    let unchanged: bool = one(
        &format!("SELECT merge(doc, {empty})::bytea = doc::bytea FROM probe"),
        &[],
    );
    assert!(unchanged);
    let contained: bool = one(
        &format!("SELECT automerge_contains(doc, {empty}) FROM probe"),
        &[],
    );
    assert!(contained);
}
