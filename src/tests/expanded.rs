// Expanded values: in-place merges, read-only references, flattening,
// memory, and the merge support function.

/// A chain of `n` small change sets on top of `doc`: the base's stored
/// bytes, the change sets (bare change chunks), and the stored bytes
/// after each step as the flat path computes them (`expected[0]` is the
/// base). Loaded into temp tables `ex_base(doc)` and
/// `ex(k, c, expected)`.
fn expanded_fixture(mut doc: AutoCommit, n: usize) -> (Vec<u8>, Vec<Vec<u8>>, Vec<Vec<u8>>) {
    doc.commit();
    let base = am::normalize(&doc.save()).unwrap();
    let mut writer = doc.fork().with_actor(actor(9));
    let mut changes = Vec::new();
    let mut expected = vec![base.clone()];
    for k in 1..=n {
        let heads = writer.get_heads();
        writer.put(ROOT, "step", k as i64).unwrap();
        writer.put(ROOT, format!("k{k}"), k as i64).unwrap();
        writer.commit();
        let c = writer.save_after(&heads);
        let next = stored_merge_changes(expected.last().unwrap(), &c);
        changes.push(c);
        expected.push(next);
    }
    Spi::run("CREATE TEMP TABLE ex_base (doc automerge NOT NULL)").unwrap();
    Spi::run_with_args(
        "INSERT INTO ex_base VALUES ($1::automerge)",
        &[base.clone().into()],
    )
    .unwrap();
    Spi::run("CREATE TEMP TABLE ex (k int PRIMARY KEY, c bytea, expected bytea)").unwrap();
    for k in 1..=n {
        Spi::run_with_args(
            "INSERT INTO ex VALUES ($1, $2, $3)",
            &[
                (k as i32).into(),
                changes[k - 1].clone().into(),
                expected[k].clone().into(),
            ],
        )
        .unwrap();
    }
    (base, changes, expected)
}

fn in_place_merges() -> usize {
    crate::expanded::IN_PLACE_MERGES.load(std::sync::atomic::Ordering::Relaxed)
}

fn expanded_contexts() -> i64 {
    one(
        "SELECT count(*) FROM pg_backend_memory_contexts \
         WHERE name = 'automerge expanded document'",
        &[],
    )
}

#[pg_test]
fn expanded_plpgsql_loops_merge_in_place_and_store_flat_bytes() {
    let (_, _, expected) = expanded_fixture(sample(), 20);
    // A local variable, referenced once: PL/pgSQL hands the R/W pointer
    // over by itself ("transfer"). The same through the operator.
    Spi::run(
        "CREATE FUNCTION pg_temp.fold(n int) RETURNS automerge LANGUAGE plpgsql AS $$
         DECLARE d automerge; ch bytea;
         BEGIN
             SELECT doc INTO d FROM ex_base;
             FOR ch IN SELECT c FROM ex WHERE k <= n ORDER BY k LOOP
                 d := merge(d, ch);
             END LOOP;
             RETURN d;
         END $$;
         CREATE FUNCTION pg_temp.fold_op(n int) RETURNS automerge LANGUAGE plpgsql AS $$
         DECLARE d automerge; ch bytea;
         BEGIN
             SELECT doc INTO d FROM ex_base;
             FOR ch IN SELECT c FROM ex WHERE k <= n ORDER BY k LOOP
                 d := d || ch;
             END LOOP;
             RETURN d;
         END $$;
         -- Each merge in a block with an EXCEPTION clause: the variable
         -- is not local to that block, so only merge's support function
         -- lets PL/pgSQL pass it read-write (\"in place\").
         CREATE FUNCTION pg_temp.fold_guarded(n int) RETURNS automerge LANGUAGE plpgsql AS $$
         DECLARE d automerge; ch bytea;
         BEGIN
             SELECT doc INTO d FROM ex_base;
             FOR ch IN SELECT c FROM ex WHERE k <= n ORDER BY k LOOP
                 BEGIN
                     d := merge(d, ch);
                 EXCEPTION WHEN invalid_text_representation THEN
                     RAISE;
                 END;
             END LOOP;
             RETURN d;
         END $$;",
    )
    .unwrap();
    for f in ["fold", "fold_op", "fold_guarded"] {
        for n in [1usize, 2, 20] {
            let before = in_place_merges();
            let bytes: Vec<u8> = one(&format!("SELECT pg_temp.{f}({n})::bytea"), &[]);
            assert_eq!(bytes, expected[n], "{f}({n})");
            // The first merge reads the flat value and returns a new
            // object; every later one replaces the document in place.
            assert_eq!(in_place_merges() - before, n - 1, "{f}({n})");
        }
    }
    // Stored into a table, sent as text, read as jsonb: all the flat
    // path's bytes and state.
    Spi::run("CREATE TEMP TABLE ex_out AS SELECT pg_temp.fold(20) AS doc").unwrap();
    let same: bool = one(
        "SELECT doc::bytea = (SELECT expected FROM ex WHERE k = 20) \
            AND pg_temp.fold(20)::text = doc::text \
            AND pg_temp.fold(20)::jsonb = doc::jsonb \
            AND pg_temp.fold(20)::jsonb->>'step' = '20' FROM ex_out",
        &[],
    );
    assert!(same);
}

#[pg_test]
fn expanded_read_only_references_are_never_modified() {
    let (_, _, expected) = expanded_fixture(sample(), 4);
    Spi::run(
        "CREATE FUNCTION pg_temp.check(e bytea[]) RETURNS boolean LANGUAGE plpgsql AS $$
         DECLARE base automerge; d automerge; x automerge; y automerge; z automerge;
                 c1 bytea; c2 bytea; c3 bytea; c4 bytea;
         BEGIN
             SELECT doc INTO base FROM ex_base;
             SELECT c INTO c1 FROM ex WHERE k = 1;
             SELECT c INTO c2 FROM ex WHERE k = 2;
             SELECT c INTO c3 FROM ex WHERE k = 3;
             SELECT c INTO c4 FROM ex WHERE k = 4;
             d := merge(base, c1);            -- a new expanded object
             ASSERT base::bytea = e[1], 'base was modified';
             x := d;                          -- a copy, not an alias
             d := merge(d, c2);               -- in place
             ASSERT x::bytea = e[2], 'alias x changed with d';
             ASSERT d::bytea = e[3], 'd after c2';
             -- d passed read-only (the target is another variable),
             -- twice: d stays as it is, both results are new objects.
             y := merge(d, c3);
             z := merge(d, c3);
             ASSERT d::bytea = e[3], 'd modified through a read-only reference';
             ASSERT y::bytea = e[4] AND z::bytea = e[4], 'y, z';
             ASSERT automerge_heads(y) = automerge_heads(z), 'heads y, z';
             -- The same object as both arguments, and as its own bytes.
             d := merge(d, d);
             ASSERT d::bytea = e[3], 'merge(d, d)';
             d := merge(d, d::bytea);
             ASSERT d::bytea = e[3], 'merge(d, d::bytea)';
             d := d || d;
             ASSERT d::bytea = e[3], 'd || d';
             -- A merge of d into another variable's expression.
             y := merge(merge(d, c3), c4);
             ASSERT d::bytea = e[3] AND y::bytea = e[5], 'nested';
             -- Reads of an expanded variable.
             ASSERT d->>'step' = '2', 'jsonb read';
             ASSERT automerge_contains(y, d) AND NOT automerge_contains(d, y), 'contains';
             ASSERT automerge_contains(d, c2) AND NOT automerge_contains(d, c3), 'contains bytea';
             ASSERT automerge_change_count(d) = automerge_change_count(d::bytea::automerge), 'count';
             RETURN true;
         END $$;",
    )
    .unwrap();
    let ok: bool = one("SELECT pg_temp.check($1)", &[expected.clone().into()]);
    assert!(ok);
}

#[pg_test]
fn expanded_failed_merge_leaves_the_variable_unchanged() {
    let (_, changes, expected) = expanded_fixture(sample(), 3);
    let mut orphan_source = sample().fork().with_actor(actor(9));
    orphan_source.put(ROOT, "a", 1i64).unwrap();
    orphan_source.commit();
    let mid = orphan_source.get_heads();
    orphan_source.put(ROOT, "b", 2i64).unwrap();
    orphan_source.commit();
    let orphan = orphan_source.save_after(&mid);
    // Actor 9's first change again, with other content: duplicate seq.
    let mut dup = sample().fork().with_actor(actor(9));
    dup.put(ROOT, "other", true).unwrap();
    dup.commit();
    let duplicate = dup.save_after(&sample().get_heads());
    let mut flipped = changes[2].clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 0x55;
    Spi::run(
        "CREATE FUNCTION pg_temp.try_bad(bad bytea[], e bytea[]) RETURNS int LANGUAGE plpgsql AS $$
         DECLARE d automerge; failed int := 0; b bytea; c1 bytea; c2 bytea; c3 bytea;
         BEGIN
             SELECT c INTO c1 FROM ex WHERE k = 1;
             SELECT c INTO c2 FROM ex WHERE k = 2;
             SELECT c INTO c3 FROM ex WHERE k = 3;
             SELECT doc INTO d FROM ex_base;
             d := merge(d, c1);  -- a new expanded object
             d := merge(d, c2);  -- in place
             d := merge(d, c2);  -- nothing new
             FOREACH b IN ARRAY bad LOOP
                 BEGIN
                     d := merge(d, b);
                 EXCEPTION WHEN invalid_text_representation THEN
                     failed := failed + 1;
                 END;
                 ASSERT d::bytea = e[3], 'd changed by a failed merge';
                 ASSERT automerge_heads(d) = automerge_heads(e[3]::automerge), 'heads';
             END LOOP;
             d := merge(d, c3);  -- in place
             ASSERT d::bytea = e[4], 'good merge after failures';
             RETURN failed;
         END $$;",
    )
    .unwrap();
    let before = in_place_merges();
    let failed: i32 = one(
        "SELECT pg_temp.try_bad($1, $2)",
        &[
            vec![
                orphan,
                duplicate,
                b"garbage".to_vec(),
                flipped,
                changes[2][..10].to_vec(),
            ]
            .into(),
            expected.clone().into(),
        ],
    );
    assert_eq!(failed, 5);
    // The failed merges were handed the variable read-write (the
    // support function's in-place path) and replaced nothing.
    assert_eq!(in_place_merges() - before, 2);
}

#[pg_test]
fn expanded_values_are_flattened_fresh_after_every_merge() {
    // The stale-flatten regression: store, merge in place, store again.
    let (_, _, expected) = expanded_fixture(sample(), 3);
    Spi::run(
        "CREATE TEMP TABLE ex_store (id int PRIMARY KEY, doc automerge);
         CREATE FUNCTION pg_temp.store_steps() RETURNS void LANGUAGE plpgsql AS $$
         DECLARE d automerge; copy automerge; c1 bytea; c2 bytea; c3 bytea;
         BEGIN
             SELECT c INTO c1 FROM ex WHERE k = 1;
             SELECT c INTO c2 FROM ex WHERE k = 2;
             SELECT c INTO c3 FROM ex WHERE k = 3;
             SELECT doc INTO d FROM ex_base;
             d := merge(d, c1);
             INSERT INTO ex_store VALUES (1, d);  -- flattens d
             copy := d;                           -- a flat copy
             d := merge(d, c2);                   -- in place
             INSERT INTO ex_store VALUES (2, d);
             d := merge(d, c3);
             UPDATE ex_store SET doc = d WHERE id = 1;
             INSERT INTO ex_store VALUES (3, copy);
         END $$;",
    )
    .unwrap();
    Spi::run("SELECT pg_temp.store_steps()").unwrap();
    for (id, step) in [(1, 3usize), (2, 2), (3, 1)] {
        let bytes: Vec<u8> = one(
            "SELECT doc::bytea FROM ex_store WHERE id = $1",
            &[id.into()],
        );
        assert_eq!(bytes, expected[step], "row {id}");
    }
}

#[pg_test]
fn expanded_values_in_toasted_columns() {
    // Big enough to be compressed and stored out of line.
    let mut doc = AutoCommit::new().with_actor(actor(1));
    let text = doc.put_object(ROOT, "text", ObjType::Text).unwrap();
    doc.splice_text(&text, 0, 0, &"lorem ipsum dolor ".repeat(20_000))
        .unwrap();
    let (base, _, expected) = expanded_fixture(doc, 3);
    Spi::run(
        "CREATE TEMP TABLE ex_big (id int PRIMARY KEY, doc automerge);
         INSERT INTO ex_big SELECT 1, doc FROM ex_base;
         CREATE FUNCTION pg_temp.big() RETURNS void LANGUAGE plpgsql AS $$
         DECLARE d automerge; ch bytea;
         BEGIN
             SELECT doc INTO d FROM ex_big WHERE id = 1;
             FOR ch IN SELECT c FROM ex ORDER BY k LOOP
                 d := merge(d, ch);
             END LOOP;
             UPDATE ex_big SET doc = d WHERE id = 1;
             INSERT INTO ex_big VALUES (2, d);
         END $$;",
    )
    .unwrap();
    Spi::run("SELECT pg_temp.big()").unwrap();
    let (bytes, size): (Option<Vec<u8>>, Option<i32>) =
        Spi::get_two("SELECT doc::bytea, pg_column_size(doc) FROM ex_big WHERE id = 1")
            .unwrap();
    assert_eq!(bytes.unwrap(), expected[3]);
    assert!(
        (size.unwrap() as usize) < base.len() / 4,
        "compressed: {size:?}"
    );
    let same: bool = one(
        "SELECT a.doc::bytea = b.doc::bytea FROM ex_big a, ex_big b WHERE a.id = 1 AND b.id = 2",
        &[],
    );
    assert!(same);
    // An UPDATE's merge result is stored the same way.
    Spi::run("UPDATE ex_big SET doc = merge(doc, (SELECT c FROM ex WHERE k = 3)) WHERE id = 1")
        .unwrap();
    let bytes: Vec<u8> = one("SELECT doc::bytea FROM ex_big WHERE id = 1", &[]);
    assert_eq!(bytes, expected[3]);
}

#[pg_test]
fn expanded_results_of_nested_merges_and_merge_agg() {
    let (_, changes, expected) = expanded_fixture(sample(), 3);
    let hex = |b: &[u8]| am::encoding::to_hex_literal(b);
    let same: bool = one(
        "SELECT merge(merge(merge(b.doc, c1.c), c2.c), c3.c)::bytea = c3.expected
            AND (b.doc || c1.c || c2.c || c3.c)::bytea = c3.expected
            AND merge(b.doc, c1.c)::text = $1
            AND merge(b.doc, c1.c)::jsonb->>'step' = '1'
            AND automerge_heads(merge(b.doc, c1.c)) = automerge_heads(c1.expected::automerge)
         FROM ex_base b, ex c1, ex c2, ex c3 WHERE c1.k = 1 AND c2.k = 2 AND c3.k = 3",
        &[hex(&expected[1]).into()],
    );
    assert!(same);
    // Every read function gives the same answer for an expanded value
    // (a merge result) as for the stored one.
    let same: bool = one(
        "WITH v AS (SELECT merge(c1.expected::automerge, c2.c) AS m, c2.expected::automerge AS f
                    FROM ex c1, ex c2 WHERE c1.k = 1 AND c2.k = 2)
         SELECT automerge_heads(m) = automerge_heads(f)
            AND automerge_change_count(m) = automerge_change_count(f)
            AND automerge_changes_bytes(m) = automerge_changes_bytes(f)
            AND automerge_changes_bytes(m, automerge_heads(b.doc)) = automerge_changes_bytes(f, automerge_heads(b.doc))
            AND (SELECT array_agg(hash) FROM automerge_changes_meta(m)) = (SELECT array_agg(hash) FROM automerge_changes_meta(f))
            AND (SELECT array_agg(change) FROM automerge_changes(m)) = (SELECT array_agg(change) FROM automerge_changes(f))
            AND automerge_get_change(m, (automerge_heads(f))[1]) = automerge_get_change(f, (automerge_heads(f))[1])
            AND automerge_to_jsonb(m, automerge_heads(b.doc)) = automerge_to_jsonb(f, automerge_heads(b.doc))
            AND automerge_contains(m, f) AND automerge_contains(f, m)
            AND automerge_contains(m, b.doc) AND NOT automerge_contains(b.doc, m)
            AND automerge_send(m) = automerge_send(f)
         FROM v, ex_base b",
        &[],
    );
    assert!(same);
    // merge_agg returns an expanded value when it built a new document.
    let mut forks = Vec::new();
    for i in 0..4u8 {
        let mut fork = sample().fork().with_actor(actor(20 + i));
        fork.put(ROOT, format!("fork{i}"), true).unwrap();
        forks.push(am::normalize(&fork.save()).unwrap());
    }
    let flat = stored_merge_all(&forks);
    Spi::run("CREATE TEMP TABLE ex_forks (doc automerge)").unwrap();
    for f in &forks {
        Spi::run_with_args(
            "INSERT INTO ex_forks VALUES ($1::automerge)",
            &[f.clone().into()],
        )
        .unwrap();
    }
    let (bytes, json): (Option<Vec<u8>>, Option<JsonB>) =
        Spi::get_two("SELECT merge_agg(doc)::bytea, merge_agg(doc)::jsonb FROM ex_forks")
            .unwrap();
    assert_eq!(bytes.unwrap(), flat);
    assert_eq!(json.unwrap().0, stored_json(&flat));
    // merge_agg of expanded inputs (merge results: step k - 1 plus
    // change set k).
    let bytes: Vec<u8> = one(
        "SELECT merge_agg(merge(p.doc, e.c) ORDER BY e.k)::bytea
         FROM (SELECT 1 AS k, doc FROM ex_base
               UNION ALL SELECT k + 1, expected::automerge FROM ex) p
         JOIN ex e USING (k)",
        &[],
    );
    let mut steps = Vec::new();
    for (k, c) in changes.iter().enumerate() {
        let step = stored_merge_changes(&expected[k], c);
        assert_eq!(step, expected[k + 1]);
        steps.push(step);
    }
    assert_eq!(bytes, stored_merge_all(&steps));
}

#[pg_test]
fn expanded_memory_is_released() {
    let (_, _, expected) = expanded_fixture(sample(), 200);
    let live_before = am::loaded::live_count();
    assert_eq!(expanded_contexts(), 0);
    Spi::run(
        "CREATE FUNCTION pg_temp.churn() RETURNS bigint LANGUAGE plpgsql AS $$
         DECLARE d automerge; x automerge; ch bytea; contexts bigint;
                 cs bytea[] := ARRAY(SELECT c FROM ex ORDER BY k);
         BEGIN
             SELECT doc INTO d FROM ex_base;
             -- In place, 200 times.
             FOR ch IN SELECT c FROM ex ORDER BY k LOOP
                 d := merge(d, ch);
             END LOOP;
             -- A new object into x each time (d is read-only here); the
             -- previous x is freed on assignment.
             FOR i IN 1..300 LOOP
                 x := merge(d, cs[1 + i % 200]) || d;
                 x := merge(x, d);
             END LOOP;
             SELECT count(*) INTO contexts FROM pg_backend_memory_contexts
             WHERE name = 'automerge expanded document';
             ASSERT d::bytea = (SELECT expected FROM ex WHERE k = 200);
             RETURN contexts;
         END $$;",
    )
    .unwrap();
    for _ in 0..3 {
        let contexts: i64 = one("SELECT pg_temp.churn()", &[]);
        // d, and x (a flat copy or an expanded value), at most.
        assert!(
            contexts <= 2,
            "{contexts} expanded objects alive in the loop"
        );
        assert_eq!(expanded_contexts(), 0);
        assert_eq!(am::loaded::live_count(), live_before);
    }
    // Statements: nested merges and merge_agg over many rows.
    let n: i64 = one(
        "SELECT count(*) FROM (
             SELECT automerge_heads(merge(merge(p.doc, e.c), e.c))
             FROM (SELECT 1 AS k, doc FROM ex_base
                   UNION ALL SELECT k + 1, expected::automerge FROM ex) p
             JOIN ex e USING (k)
         ) s",
        &[],
    );
    assert_eq!(n, 200);
    let bytes: Vec<u8> = one(
        "SELECT merge_agg(merge(p.doc, e.c) ORDER BY e.k)::bytea
         FROM (SELECT 1 AS k, doc FROM ex_base
               UNION ALL SELECT k + 1, expected::automerge FROM ex) p
         JOIN ex e USING (k)",
        &[],
    );
    assert_eq!(bytes.len(), expected[200].len());
    assert_eq!(expanded_contexts(), 0);
    assert_eq!(am::loaded::live_count(), live_before);
}

#[pg_test]
fn merge_support_function_is_attached_and_answers_modify_in_place() {
    let attached: String = one(
        "SELECT string_agg(DISTINCT prosupport::regproc::text, ',') FROM pg_proc \
         WHERE proname = 'merge' AND prorettype = 'automerge'::regtype",
        &[],
    );
    assert_eq!(attached, "automerge_merge_support");
    let labels: String = one(
        "SELECT string_agg(DISTINCT provolatile::text || proisstrict::text || proparallel::text, ',') \
         FROM pg_proc WHERE oid IN ('merge(automerge, automerge)'::regprocedure, \
           'merge(automerge, bytea)'::regprocedure, 'automerge_merge_support(internal)'::regprocedure)",
        &[],
    );
    assert_eq!(labels, "itrues");

    // The support function itself: a Param of the target variable as
    // the first argument is named; anything else is not.
    // SAFETY: nodes built in the current memory context, as the
    // planner would pass them.
    unsafe {
        let param = |id: i32| {
            let p = pg_sys::palloc0(size_of::<pg_sys::Param>()).cast::<pg_sys::Param>();
            (*p).xpr.type_ = pg_sys::NodeTag::T_Param;
            (*p).paramkind = pg_sys::ParamKind::PARAM_EXTERN;
            (*p).paramid = id;
            p.cast::<pg_sys::Node>()
        };
        let request = |args: Vec<*mut pg_sys::Node>, paramid: i32| {
            let r = pg_sys::palloc0(size_of::<pg_sys::SupportRequestModifyInPlace>())
                .cast::<pg_sys::SupportRequestModifyInPlace>();
            (*r).type_ = pg_sys::NodeTag::T_SupportRequestModifyInPlace;
            let mut list: *mut pg_sys::List = std::ptr::null_mut();
            for a in args {
                list = pg_sys::lappend(list, a.cast());
            }
            (*r).args = list;
            (*r).paramid = paramid;
            r.cast::<pg_sys::Node>()
        };
        let target = param(1);
        assert_eq!(
            crate::merge::modify_in_place_param(request(vec![target, param(2)], 1)),
            target
        );
        // Other references to the variable are fine (merge copes).
        let target = param(1);
        assert_eq!(
            crate::merge::modify_in_place_param(request(vec![target, param(1)], 1)),
            target
        );
        assert!(crate::merge::modify_in_place_param(request(vec![param(2), param(1)], 1)).is_null());
        assert!(crate::merge::modify_in_place_param(request(vec![], 1)).is_null());
        let c = pg_sys::palloc0(size_of::<pg_sys::Const>()).cast::<pg_sys::Node>();
        (*c).type_ = pg_sys::NodeTag::T_Const;
        assert!(crate::merge::modify_in_place_param(request(vec![c, param(1)], 1)).is_null());
        let exec = param(1);
        (*exec.cast::<pg_sys::Param>()).paramkind = pg_sys::ParamKind::PARAM_EXEC;
        assert!(crate::merge::modify_in_place_param(request(vec![exec, param(1)], 1)).is_null());
        let other =
            pg_sys::palloc0(size_of::<pg_sys::SupportRequestSimplify>()).cast::<pg_sys::Node>();
        (*other).type_ = pg_sys::NodeTag::T_SupportRequestSimplify;
        assert!(crate::merge::modify_in_place_param(other).is_null());
        assert!(crate::merge::modify_in_place_param(std::ptr::null_mut()).is_null());
    }
}
