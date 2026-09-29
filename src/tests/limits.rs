// The load memory limit, pg_automerge.max_load_memory (see docs/DESIGN.md,
// "Resource limits"): its default and privileges, 53400 with DETAIL and
// HINT on every SQL path that takes client bytes or builds a merge
// result, stored values that stay readable when the limit is lowered,
// -1, and bundle chunks.

/// A document of one list with `n` items by actor `a`, in one change.
fn list_doc(n: usize, a: u8) -> AutoCommit {
    let mut doc = AutoCommit::new().with_actor(actor(a));
    let list = doc.put_object(ROOT, "l", ObjType::List).unwrap();
    for i in 0..n {
        doc.insert(&list, i, i as i64).unwrap();
    }
    doc.commit();
    doc
}

fn hex(bytes: &[u8]) -> String {
    am::encoding::to_hex_literal(bytes)
}

/// The estimated load of client bytes on their own.
fn load_estimate(bytes: &[u8]) -> u64 {
    am::budget::scan_input_exact(bytes, None).load_estimate()
}

/// The estimated load of a stored value.
fn stored_estimate(bytes: &[u8]) -> u64 {
    am::budget::doc_estimate(&am::budget::scan_doc_exact(bytes))
}

/// `SET LOCAL pg_automerge.max_load_memory` to `kb` kB (-1: no limit).
fn set_max_load_memory(kb: i64) {
    Spi::run(&format!("SET LOCAL pg_automerge.max_load_memory = {kb}")).unwrap();
}

const LIMIT_HINT: &str = "A superuser can raise \"pg_automerge.max_load_memory\".";

/// `sql` fails with 53400, a primary message starting with `what` and
/// naming the limit `shown`, a DETAIL starting with `detail`, and the
/// HINT.
fn assert_over_limit(sql: &str, what: &str, shown: &str, detail: &str) {
    let [code, message, got_detail, hint]: [String; 4] = sql_error_report(sql).try_into().unwrap();
    let short = &sql[..sql.len().min(120)];
    assert_eq!(code, "53400", "{short}: {message}");
    assert_eq!(
        message,
        format!("{what} exceeds \"pg_automerge.max_load_memory\" ({shown})"),
        "{short}"
    );
    assert!(got_detail.starts_with(detail), "{short}: {got_detail}");
    assert_eq!(hint, LIMIT_HINT, "{short}");
}

const INPUT: &str = "estimated memory to load automerge input";
const MERGED: &str = "estimated memory to load merged automerge document";

#[pg_test]
fn max_load_memory_is_a_superuser_setting_of_2gb() {
    let shown: String = one("SELECT current_setting('pg_automerge.max_load_memory')", &[]);
    assert_eq!(shown, "2GB");
    let row: String = one(
        "SELECT concat_ws(' ', context, unit, boot_val, min_val) FROM pg_settings \
         WHERE name = 'pg_automerge.max_load_memory'",
        &[],
    );
    assert_eq!(row, "superuser kB 2097152 -1");
    // Only superusers (or roles granted SET on it) may change it.
    Spi::run("CREATE ROLE lim_plain").unwrap();
    for value in ["-1", "'4GB'", "'1MB'"] {
        assert_eq!(
            sql_error(&format!(
                "DO $$ BEGIN SET LOCAL ROLE lim_plain; \
                 SET LOCAL pg_automerge.max_load_memory = {value}; END $$"
            )),
            "42501: permission denied to set parameter \"pg_automerge.max_load_memory\""
        );
    }
    Spi::run("GRANT SET ON PARAMETER pg_automerge.max_load_memory TO lim_plain").unwrap();
    Spi::run(
        "DO $$ BEGIN SET LOCAL ROLE lim_plain; \
         SET LOCAL pg_automerge.max_load_memory = '1MB'; END $$",
    )
    .unwrap();
    Spi::run("REVOKE SET ON PARAMETER pg_automerge.max_load_memory FROM lim_plain").unwrap();
}

#[pg_test]
fn client_input_over_the_limit_is_53400_on_every_path() {
    let mut big = list_doc(4_000, 1);
    let save = big.save();
    let plain = big.document().save_nocompress();
    let chunks = big.document().save_after(&[]);
    for bytes in [&save, &plain, &chunks] {
        assert!(load_estimate(bytes) > 1 << 20);
    }
    let small = list_doc(10, 2).document().save_nocompress();
    Spi::run("CREATE TEMP TABLE lim (id int PRIMARY KEY, doc automerge NOT NULL)").unwrap();
    Spi::run_with_args("INSERT INTO lim VALUES (1, $1::automerge)", &[small.clone().into()]).unwrap();
    Spi::run("CREATE TEMP TABLE lim_in (doc automerge)").unwrap();
    set_max_load_memory(1024);

    let loading = "Loading it could take up to ";
    for bytes in [&save, &plain, &chunks] {
        let h = hex(bytes);
        for sql in [
            // Text input (also what COPY and a dump restore use).
            format!("SELECT '{h}'::automerge"),
            format!("INSERT INTO lim_in VALUES ('{h}')"),
            // The bytea cast.
            format!("SELECT '{h}'::bytea::automerge"),
            format!("INSERT INTO lim_in VALUES ('{h}'::bytea)"),
            // merge(automerge, bytea), both spellings.
            format!("UPDATE lim SET doc = merge(doc, '{h}'::bytea)"),
            format!("UPDATE lim SET doc = doc || '{h}'::bytea"),
            // In PL/pgSQL.
            format!(
                "DO $$ DECLARE d automerge; BEGIN \
                   SELECT doc INTO d FROM lim WHERE id = 1; \
                   d := merge(d, '{h}'::bytea); END $$"
            ),
            // merge(automerge, automerge) of a literal: its input.
            format!("SELECT merge(doc, '{h}') FROM lim"),
        ] {
            assert_over_limit(&sql, INPUT, "1 MB", loading);
        }
    }
    // automerge_contains(doc, bytea) when it has to load doc ++ changes.
    assert_over_limit(
        &format!("SELECT automerge_contains(doc, '{}'::bytea) FROM lim", hex(&chunks)),
        INPUT,
        "1 MB",
        loading,
    );

    // Binary input (binary COPY, and what drivers send for binary
    // parameters) and text COPY, from files written with no limit.
    let dir = std::env::temp_dir();
    let (bin, txt) = (
        dir.join(format!("pg_automerge_limits_{}.bin", std::process::id())),
        dir.join(format!("pg_automerge_limits_{}.txt", std::process::id())),
    );
    set_max_load_memory(-1);
    Spi::run_with_args("CREATE TEMP TABLE lim_bytes AS SELECT $1::bytea AS b", &[save.clone().into()])
        .unwrap();
    Spi::run(&format!("COPY lim_bytes TO '{}' (FORMAT binary)", bin.display())).unwrap();
    Spi::run(&format!("COPY lim_bytes TO '{}'", txt.display())).unwrap();
    set_max_load_memory(1024);
    for (file, options) in [(&bin, " (FORMAT binary)"), (&txt, "")] {
        assert_over_limit(
            &format!("COPY lim_in FROM '{}'{options}", file.display()),
            INPUT,
            "1 MB",
            loading,
        );
    }
    // Within the limit, the same files load.
    set_max_load_memory(-1);
    for (file, options) in [(&bin, " (FORMAT binary)"), (&txt, "")] {
        Spi::run(&format!("COPY lim_in FROM '{}'{options}", file.display())).unwrap();
    }
    let _ = std::fs::remove_file(&bin);
    let _ = std::fs::remove_file(&txt);
    let n: i64 = one("SELECT count(*) FROM lim_in", &[]);
    assert_eq!(n, 2);

    // A literal too long for the limit whatever it holds is refused
    // before its hex is decoded: 100 kB of bytes under a 64 kB limit.
    set_max_load_memory(64);
    assert_over_limit(
        &format!("SELECT '\\x{}'::automerge", "00".repeat(100_000)),
        INPUT,
        "64 kB",
        "Loading it could take at least ",
    );
    // A deflate bomb (a compressed change chunk inflating to 8 MB of
    // zeros) stops the scan early: "at least".
    let bomb = {
        let deflated = am::test_hooks::deflate(&vec![0u8; 8 << 20]);
        let mut out = vec![0x85, 0x6f, 0x4a, 0x83, 0, 0, 0, 0, 2];
        let mut n = deflated.len();
        while n >= 0x80 {
            out.push((n & 0x7f) as u8 | 0x80);
            n >>= 7;
        }
        out.push(n as u8);
        out.extend(deflated);
        out
    };
    set_max_load_memory(1024);
    assert_over_limit(
        &format!("SELECT '{}'::bytea::automerge", hex(&bomb)),
        INPUT,
        "1 MB",
        "Loading it could take at least ",
    );
    // A change chunk whose header lists the empty actor id `others`
    // times (one byte each; Automerge keeps every entry): priced per
    // entry, 110 bytes. 9,000 entries are 990 kB: read, and with the
    // rest over 1 MB, "up to" (priced by name, as the first version did,
    // it was 160 kB); 20,000 are more than the limit pays for, so the scan
    // stops before reading them: "at least".
    let listing = |others: usize| {
        let mut data = vec![0u8, 16];
        data.extend([9u8; 16]);
        data.extend([1, 1, 0, 0]); // seq, start op, time, message
        let mut n = others;
        while n >= 0x80 {
            data.push((n & 0x7f) as u8 | 0x80);
            n >>= 7;
        }
        data.push(n as u8);
        data.extend(std::iter::repeat_n(0u8, others));
        data.push(0); // no columns
        let mut out = vec![0x85, 0x6f, 0x4a, 0x83, 0, 0, 0, 0, 1];
        let mut n = data.len();
        while n >= 0x80 {
            out.push((n & 0x7f) as u8 | 0x80);
            n >>= 7;
        }
        out.push(n as u8);
        out.extend(data);
        out
    };
    for (others, detail) in [(9_000, "Loading it could take up to "), (20_000, "Loading it could take at least ")] {
        let h = hex(&listing(others));
        for sql in [
            format!("SELECT '{h}'::automerge"),
            format!("UPDATE lim SET doc = merge(doc, '{h}'::bytea)"),
            format!("SELECT automerge_contains(doc, '{h}'::bytea) FROM lim"),
        ] {
            assert_over_limit(&sql, INPUT, "1 MB", detail);
        }
    }
    // Nothing was written.
    let unchanged: bool = one("SELECT doc::bytea = $1 FROM lim", &[small.into()]);
    assert!(unchanged);
}

#[pg_test]
fn merge_results_over_the_limit_are_53400() {
    // a: 2000 items; b: a plus 1000 more (one change); c: a plus a small
    // change by another actor (concurrent with b).
    let mut doc = list_doc(2_000, 1);
    let a = doc.document().save_nocompress();
    let heads_a = doc.get_heads();
    let list = pg_automerge_core::automerge::ReadDoc::get(&doc, ROOT, "l")
        .unwrap()
        .unwrap()
        .1;
    for i in 0..1_000 {
        doc.insert(&list, 2_000 + i, -(i as i64)).unwrap();
    }
    doc.commit();
    let change = doc.document().save_after(&heads_a);
    let b = doc.document().save_nocompress();
    let mut other = AutoCommit::load(&a).unwrap().with_actor(actor(2));
    other.put(ROOT, "other", true).unwrap();
    other.commit();
    let c = other.document().save_nocompress();
    let c_change = other.document().save_after(&heads_a);

    // A limit that each write fits and the result does not.
    let base = am::budget::scan_doc(&a);
    let apply =
        am::budget::scan_input_exact(&change, None).apply_estimate(am::budget::Base::from(&base));
    let limit = stored_estimate(&a).max(apply).max(load_estimate(&c_change)) + (8 << 10);
    assert!(stored_estimate(&b) > limit);
    let limit_kb = limit.div_ceil(1024) as i64;
    let shown = format!("{limit_kb} kB");

    Spi::run("CREATE TEMP TABLE lm (id int PRIMARY KEY, doc automerge NOT NULL)").unwrap();
    for (id, bytes) in [(1, &a), (2, &b), (3, &c)] {
        Spi::run_with_args("INSERT INTO lm VALUES ($1, $2::automerge)", &[id.into(), bytes.clone().into()])
            .unwrap();
    }
    set_max_load_memory(limit_kb);
    let loading = "Loading it could take up to ";
    for sql in [
        // Incremental changes that grow the document past the limit.
        format!("UPDATE lm SET doc = merge(doc, '{}'::bytea) WHERE id = 1", hex(&change)),
        format!("SELECT merge(doc, '{}'::bytea) FROM lm WHERE id = 1", hex(&change)),
        // Two stored documents, each loaded as they are, merged.
        "SELECT merge(x.doc, y.doc) FROM lm x, lm y WHERE x.id = 2 AND y.id = 3".into(),
        "SELECT x.doc || y.doc FROM lm x, lm y WHERE x.id = 3 AND y.id = 2".into(),
        "SELECT merge_agg(doc ORDER BY id) FROM lm WHERE id IN (2, 3)".into(),
        "SELECT merge_agg(doc ORDER BY id DESC) FROM lm".into(),
        // A PL/pgSQL chain: each step is small, the result grows.
        format!(
            "DO $$ DECLARE d automerge; BEGIN \
               SELECT doc INTO d FROM lm WHERE id = 1; \
               d := merge(d, '{}'::bytea); \
               d := merge(d, '{}'::bytea); END $$",
            hex(&c_change),
            hex(&change)
        ),
    ] {
        assert_over_limit(&sql, MERGED, &shown, loading);
    }
    // A failed merge in PL/pgSQL leaves the variable as it was.
    Spi::run(
        "CREATE FUNCTION pg_temp.lim_chain(x bytea, y bytea) RETURNS bool \
         LANGUAGE plpgsql AS $$ DECLARE d automerge; h text[]; BEGIN \
           SELECT doc INTO d FROM lm WHERE id = 1; \
           d := merge(d, x); h := automerge_heads(d); \
           BEGIN d := merge(d, y); \
           EXCEPTION WHEN configuration_limit_exceeded THEN NULL; END; \
           RETURN automerge_heads(d) = h AND d::jsonb ? 'other'; END $$",
    )
    .unwrap();
    let kept: bool = one(
        "SELECT pg_temp.lim_chain($1, $2)",
        &[c_change.clone().into(), change.clone().into()],
    );
    assert!(kept);
    // The stored rows are unchanged, and with room for the result every
    // one of these merges goes through.
    let same: bool = one("SELECT doc::bytea = $1 FROM lm WHERE id = 1", &[a.clone().into()]);
    assert!(same);
    set_max_load_memory(-1);
    Spi::run(&format!("UPDATE lm SET doc = merge(doc, '{}'::bytea) WHERE id = 1", hex(&change)))
        .unwrap();
    let merged: bool = one(
        "SELECT automerge_heads(x.doc) = automerge_heads(y.doc) FROM lm x, lm y WHERE x.id = 1 AND y.id = 2",
        &[],
    );
    assert!(merged);
    let n: i64 = one("SELECT jsonb_array_length(merge_agg(doc)::jsonb->'l') FROM lm", &[]);
    assert_eq!(n, 3_000);
}

#[pg_test]
fn stored_values_stay_readable_when_the_limit_is_lowered() {
    let mut big = list_doc(4_000, 1);
    let heads = big.get_heads();
    big.put(ROOT, "status", "stored").unwrap();
    big.commit();
    let plain = big.document().save_nocompress();
    // The document before its last change, saved (compressed, and not).
    let older_doc = big.document().fork_at(&heads).unwrap();
    let older = older_doc.save();
    let older_plain = older_doc.save_nocompress();
    Spi::run("CREATE TEMP TABLE ls (id int PRIMARY KEY, doc automerge NOT NULL)").unwrap();
    Spi::run_with_args("INSERT INTO ls VALUES (1, $1::automerge)", &[plain.clone().into()]).unwrap();
    // Far below what loading it takes (but a tenth of it holds the
    // older save inflated, which parsing it takes).
    assert!(stored_estimate(&plain) > 1 << 20);
    assert!(am::budget::scan_input(&older, None).parse_estimate() < 25 << 10);
    set_max_load_memory(256);
    let h = heads.iter().map(|h| format!("'{h}'")).collect::<Vec<_>>().join(",");
    for sql in [
        "SELECT doc::jsonb->>'status' = 'stored' FROM ls".to_string(),
        "SELECT doc->>'status' = 'stored' FROM ls".into(),
        "SELECT cardinality(automerge_heads(doc)) = 1 FROM ls".into(),
        "SELECT automerge_change_count(doc) = 2 FROM ls".into(),
        "SELECT count(*) = 2 FROM ls, automerge_changes_meta(doc)".into(),
        "SELECT count(*) = 2 FROM ls, automerge_changes(doc)".into(),
        "SELECT length(automerge_changes_bytes(doc)) > 0 FROM ls".into(),
        format!("SELECT automerge_to_jsonb(doc, ARRAY[{h}]) ? 'l' FROM ls"),
        format!(
            "SELECT (automerge_get_change(doc, {})).hash IS NOT NULL FROM ls",
            h.split(',').next().unwrap()
        ),
        "SELECT doc::bytea = $1 FROM ls".into(),
        "SELECT length(doc::text) > 0 FROM ls".into(),
        // No-op writes: nothing new is loaded.
        "SELECT merge(doc, doc)::bytea = $1 FROM ls".into(),
        "SELECT merge(doc, doc::bytea)::bytea = $1 FROM ls".into(),
        "SELECT automerge_contains(doc, doc::bytea) FROM ls".into(),
        "SELECT automerge_contains(doc, doc) FROM ls".into(),
        "SELECT merge_agg(doc)::bytea = $1 FROM ls".into(),
        "SELECT merge_agg(doc)::bytea = $1 FROM (SELECT doc FROM ls UNION ALL SELECT doc FROM ls) s".into(),
    ] {
        let ok: bool = one(&sql, &[plain.clone().into()]);
        assert!(ok, "{sql}");
    }
    // An older save of it: the stored document is loaded (never
    // checked), the save only parsed: a no-op.
    for bytes in [older, older_plain] {
        let noop: bool = one(
            "SELECT merge(doc, $1::bytea)::bytea = doc::bytea AND automerge_contains(doc, $1::bytea) \
             FROM ls",
            &[bytes.into()],
        );
        assert!(noop);
    }
    Spi::run("UPDATE ls SET doc = merge(doc, doc::bytea)").unwrap();
    Spi::run("UPDATE ls SET doc = doc").unwrap();
    // But the same bytes as new input are over the limit (a dump
    // restore into a server with a lower limit needs it raised).
    assert_over_limit(
        &format!("SELECT '{}'::automerge", hex(&plain)),
        INPUT,
        "256 kB",
        "Loading it could take up to ",
    );
}

#[pg_test]
fn minus_one_disables_the_limit_but_not_the_bundle_check() {
    let mut big = list_doc(4_000, 1);
    let save = big.save();
    set_max_load_memory(1);
    assert!(sql_error(&format!("SELECT '{}'::bytea::automerge", hex(&save))).starts_with("53400: "));
    set_max_load_memory(-1);
    let n: i64 = one(
        "SELECT jsonb_array_length($1::bytea::automerge::jsonb->'l')",
        &[save.into()],
    );
    assert_eq!(n, 4_000);
    // A bundle chunk (Automerge's experimental format) is refused with
    // 0A000 whatever the limit.
    let bundle = "\\x856f4a83000000000303000000";
    for limit in [-1, 2 * 1024 * 1024] {
        set_max_load_memory(limit);
        for sql in [
            format!("SELECT '{bundle}'::automerge"),
            format!("SELECT '{bundle}'::bytea::automerge"),
        ] {
            assert_eq!(
                sql_error(&sql),
                "0A000: automerge bundle chunks are not supported",
                "{sql}"
            );
        }
    }
}
