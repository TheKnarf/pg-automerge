#!/usr/bin/env bash
# The extension's control-file flags (run via `mise run extension`, also
# part of `mise run test`). See docs/DESIGN.md, "Installation, schema and
# privileges".
#
# relocatable = true:
#   - CREATE EXTENSION .. SCHEMA ext1 puts every member object there;
#   - objects that depend on the extension (a table with an automerge
#     column, a domain over it, a STORED generated doc::jsonb column, GIN
#     and btree expression indexes, a check constraint, views using merge,
#     || and merge_agg, a BEGIN ATOMIC SQL function, automerge_notify()
#     triggers) keep working after ALTER EXTENSION pg_automerge SET SCHEMA
#     ext2: same bytes, heads and jsonb, indexes valid and used, writes
#     through the moved functions, history rows, an in-place PL/pgSQL
#     merge, the trigger notifying with a search_path that has neither
#     schema but a foreign type named automerge first;
#   - a custom-format dump of the moved extension restores (into ext2) and
#     can be moved again.
# trusted = false, superuser = true:
#   - a non-superuser that owns the database cannot CREATE EXTENSION;
#   - pg_automerge.verify_writes and pg_automerge.max_load_memory stay
#     superuser-only for that role: SET (before and after the library is
#     loaded), ALTER ROLE/DATABASE .. SET; a superuser can still set them
#     per role or GRANT SET ON PARAMETER.
#
# Env: see tests/lib.sh.

# shellcheck source=tests/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

DB=pg_automerge_ext
DB_RESTORE=pg_automerge_ext_restore
DB_OWNED=pg_automerge_ext_owned
ROLE=automerge_ext_plain
# For servers that require passwords over TCP (an external one, e.g. the
# Docker image); the pgrx cluster trusts local connections.
ROLE_PASSWORD="automerge-ext-$$"
DROP_DBS=("$DB" "$DB_RESTORE" "$DB_OWNED")

on_exit() {
    if server_running; then
        sql_on postgres -c "DROP DATABASE IF EXISTS $DB_OWNED WITH (FORCE)" -c "DROP ROLE IF EXISTS $ROLE" \
            >/dev/null 2>&1 || true
    fi
    if [[ $1 == 0 ]]; then log "all extension flag checks passed"; else echo "extension test FAILED" >&2; fi
}

log "generating fixture documents"
eval "$(cargo run -q -p pg_automerge_core --example gen_concurrency)"

install_extension
start_server
create_db

# Members of the extension not in schema $1 (types, functions, operators;
# casts have no schema), as "kind name" lines.
MISPLACED_SQL="SELECT string_agg(pg_describe_object(d.classid, d.objid, 0), '; ' ORDER BY 1)
FROM pg_depend d
LEFT JOIN pg_proc p ON d.classid = 'pg_proc'::regclass AND p.oid = d.objid
LEFT JOIN pg_type t ON d.classid = 'pg_type'::regclass AND t.oid = d.objid
LEFT JOIN pg_operator o ON d.classid = 'pg_operator'::regclass AND o.oid = d.objid
WHERE d.deptype = 'e' AND d.refclassid = 'pg_extension'::regclass
  AND d.refobjid = (SELECT oid FROM pg_extension WHERE extname = 'pg_automerge')
  AND d.classid <> 'pg_cast'::regclass
  AND coalesce(p.pronamespace, t.typnamespace, o.oprnamespace) IS DISTINCT FROM :'nsp'::regnamespace"

check_placement() {
    local db="$1" nsp="$2" what="$3"
    [[ "$(sql_on "$db" -c "SELECT extnamespace::regnamespace::text || ' ' || extrelocatable FROM pg_extension WHERE extname = 'pg_automerge'")" \
       == "$nsp true" ]] || fail "$what: extension not in $nsp or not relocatable"
    local misplaced
    misplaced="$(sql_on "$db" -v nsp="$nsp" <<<"$MISPLACED_SQL")"
    [[ -z "$misplaced" ]] || fail "$what: members outside $nsp: $misplaced"
}

log "CREATE EXTENSION .. SCHEMA ext1, objects depending on it"
sql -v base="$BASE" -v a="$NEW_A" -v b="$NEW_B" -v inc_a="$INC_A" -v many="$MANY" <<'SQL'
CREATE SCHEMA ext1;
CREATE SCHEMA ext2;
CREATE EXTENSION pg_automerge SCHEMA ext1;
CREATE SCHEMA app;
SET search_path = app, ext1;
CREATE DOMAIN app.doc_domain AS automerge;
CREATE TABLE docs (
    id int PRIMARY KEY,
    doc automerge NOT NULL CHECK (cardinality(automerge_heads(doc)) >= 1),
    extra doc_domain,
    data jsonb GENERATED ALWAYS AS (doc::jsonb) STORED
);
CREATE INDEX docs_data_gin ON docs USING gin ((doc::jsonb) jsonb_path_ops);
CREATE INDEX docs_title ON docs ((doc->>'title'));
CREATE TRIGGER docs_notify AFTER INSERT OR UPDATE OR DELETE ON docs
    FOR EACH ROW EXECUTE FUNCTION automerge_notify('docs_changed', 'id');
INSERT INTO docs (id, doc, extra) VALUES
    (1, :'base', :'a'),
    (2, :'a'::bytea::automerge, NULL),
    (3, merge(:'a'::automerge, :'b'::automerge), NULL),
    (4, :'base'::automerge || :'inc_a'::bytea, NULL),
    (5, :'many', NULL);
CREATE VIEW merged AS
    SELECT x.id, merge(x.doc, y.doc) AS m, x.doc || y.doc AS o
    FROM docs x JOIN docs y ON y.id = x.id + 1;
CREATE VIEW all_merged AS SELECT merge_agg(doc ORDER BY id) AS m FROM docs;
-- A SQL function whose body is parsed at creation (BEGIN ATOMIC) keeps
-- working after a move; a string body would resolve names at call time.
CREATE FUNCTION doc_keys(d automerge) RETURNS bigint LANGUAGE sql IMMUTABLE
BEGIN ATOMIC
    SELECT count(*) FROM jsonb_object_keys(merge(d, d)::jsonb);
END;
SQL
check_placement "$DB" ext1 "after CREATE EXTENSION .. SCHEMA ext1"

# Fingerprint of everything that depends on the extension; $1 is the
# extension's schema.
fingerprint() {
    local db="$1" nsp="$2"
    sql_on "$db" -c "SET search_path = app;
        SELECT string_agg(id || ':' || md5(doc::bytea) || ':' || $nsp.automerge_heads(doc)::text
                          || ':' || md5(doc::jsonb::text) || ':' || md5(data::text)
                          || ':' || coalesce(md5(extra::bytea), '-') || ':' || doc_keys(doc),
                          ',' ORDER BY id)
        FROM docs;
        SELECT string_agg(id || ':' || md5(m::bytea) || ':' || md5(o::jsonb::text), ',' ORDER BY id)
        FROM merged;
        SELECT md5(m::bytea) FROM all_merged;"
}

BEFORE="$(fingerprint "$DB" ext1)"

log "ALTER EXTENSION pg_automerge SET SCHEMA ext2"
sql -c "ALTER EXTENSION pg_automerge SET SCHEMA ext2"
check_placement "$DB" ext2 "after SET SCHEMA ext2"
# Nothing of the extension is left in ext1.
sql -c "DROP SCHEMA ext1 RESTRICT" || fail "schema ext1 still holds objects"
[[ "$(fingerprint "$DB" ext2)" == "$BEFORE" ]] || fail "dependent objects read differently after SET SCHEMA"
# Views and column types now name the new schema.
sql -c "SELECT pg_get_viewdef('app.merged'::regclass)" | grep -q 'OPERATOR(ext2.||)' \
    || fail "view does not reference ext2: $(sql -c "SELECT pg_get_viewdef('app.merged'::regclass)")"
[[ "$(sql -c "SELECT format_type(atttypid, NULL) FROM pg_attribute WHERE attrelid = 'app.docs'::regclass AND attname = 'doc'")" \
   == ext2.automerge ]] || fail "column type not ext2.automerge"

# $4/$5: a change set not yet in row 1 (INC_A or INC_B) and the key it sets.
check_after_move() {
    local db="$1" nsp="$2" what="$3" change="$4" key="$5"
    [[ "$(sql_on "$db" -c "SELECT count(*) FROM pg_index WHERE indrelid = 'app.docs'::regclass AND indisvalid")" == 3 ]] \
        || fail "$what: indexes missing or invalid"
    local plan
    plan="$(sql_on "$db" -c "SET search_path = app; SET enable_seqscan = off;
        EXPLAIN (COSTS OFF) SELECT id FROM docs WHERE doc::jsonb @> '{\"base\": true}'")"
    grep -q docs_data_gin <<<"$plan" || fail "$what: GIN expression index not used: $plan"
    [[ "$(sql_on "$db" -c "SET search_path = app; SET enable_seqscan = off;
        SELECT string_agg(id::text, ',' ORDER BY id) FROM docs WHERE doc::jsonb @> '{\"base\": true}'")" \
       == 1,2,3,4,5 ]] || fail "$what: index scan result differs"
    # History rows are built from the moved composite types.
    [[ "$(sql_on "$db" -c "SELECT (SELECT count(*) FROM $nsp.automerge_changes(doc) c WHERE c.change IS NOT NULL)
                                  = $nsp.automerge_change_count(doc)
                                  AND $nsp.automerge_change_count(doc) > 1
                           FROM app.docs WHERE id = 3")" == t ]] \
        || fail "$what: automerge_changes after the move"
    # An in-place PL/pgSQL merge through the moved support function.
    sql_on "$db" <<SQL >/dev/null || fail "$what: PL/pgSQL merge"
DO \$\$
DECLARE d $nsp.automerge;
BEGIN
    SELECT doc INTO d FROM app.docs WHERE id = 2;
    d := $nsp.merge(d, '$INC_B'::bytea);
    d := $nsp.merge(d, '$INC_A'::bytea);
    IF NOT (d::jsonb ? 'inc_a' AND d::jsonb ? 'inc_b') THEN RAISE 'merge lost changes'; END IF;
END \$\$;
SQL
    # The trigger fires with a search_path holding neither the extension's
    # schema nor its old one, but a foreign type named automerge first: it
    # finds the automerge columns by the type in its own schema.
    local out
    out="$(sql_on "$db" -v inc="$change" <<SQL 2>&1
CREATE SCHEMA IF NOT EXISTS decoy;
CREATE TYPE decoy.automerge AS (x int);
SET search_path = decoy, app;
LISTEN docs_changed;
UPDATE docs SET doc = $nsp.merge(doc, :'inc'::bytea), extra = $nsp.merge(extra, :'inc'::bytea) WHERE id = 1;
SELECT 1;
DROP SCHEMA decoy CASCADE;
SQL
)"
    grep -q 'Asynchronous notification "docs_changed" with payload "{"table":"app.docs","op":"UPDATE",.*"key":{"id":1},"columns":{"doc":{"heads":\[.*"extra":{"heads":\[' <<<"$out" \
        || fail "$what: the trigger did not report both automerge columns: $out"
    [[ "$(sql_on "$db" -c "SELECT data = doc::jsonb AND data->>'$key' IS NOT NULL FROM app.docs WHERE id = 1")" == t ]] \
        || fail "$what: generated column not recomputed"
    # The check constraint still runs (through the moved function).
    if sql_on "$db" -c "INSERT INTO app.docs (id, doc) VALUES (9, '\\x'::bytea)" 2>/dev/null; then
        fail "$what: check constraint did not reject the empty document"
    fi
}
check_after_move "$DB" ext2 "moved" "$INC_B" inc_b

log "custom-format dump of the moved extension, restored"
EXPECTED="$(fingerprint "$DB" ext2)"
sql_on postgres -c "DROP DATABASE IF EXISTS $DB_RESTORE WITH (FORCE)" -c "CREATE DATABASE $DB_RESTORE"
"$BINDIR/pg_dump" "${CONN[@]}" -d "$DB" -Fc -f "$WORK/dump.custom"
"$BINDIR/pg_restore" "${CONN[@]}" -d "$DB_RESTORE" --exit-on-error "$WORK/dump.custom" \
    || fail "pg_restore failed"
check_placement "$DB_RESTORE" ext2 "restored"
[[ "$(fingerprint "$DB_RESTORE" ext2)" == "$EXPECTED" ]] || fail "restored data differs"
log "moving the restored extension to public"
sql_on "$DB_RESTORE" -c "ALTER EXTENSION pg_automerge SET SCHEMA public"
check_placement "$DB_RESTORE" public "restored, then moved"
[[ "$(fingerprint "$DB_RESTORE" public)" == "$EXPECTED" ]] || fail "restored data differs after a second move"
check_after_move "$DB_RESTORE" public "restored, then moved" "$INC_A" inc_a

log "a non-superuser owning the database cannot install it (trusted = false)"
sql_on postgres -c "DROP DATABASE IF EXISTS $DB_OWNED WITH (FORCE)" -c "DROP ROLE IF EXISTS $ROLE" \
    -c "CREATE ROLE $ROLE LOGIN PASSWORD '$ROLE_PASSWORD'" -c "CREATE DATABASE $DB_OWNED OWNER $ROLE"
as_role() { PGPASSWORD="$ROLE_PASSWORD" sql_on "$DB_OWNED" -U "$ROLE" "$@"; }
if out="$(as_role -c "CREATE EXTENSION pg_automerge" 2>&1)"; then
    fail "a non-superuser installed the extension"
fi
grep -q 'permission denied to create extension "pg_automerge"' <<<"$out" || fail "unexpected error: $out"
[[ "$(sql_on "$DB_OWNED" -c "SELECT count(*) FROM pg_extension WHERE extname = 'pg_automerge'")" == 0 ]] \
    || fail "extension present after the refused install"

log "pg_automerge.verify_writes stays superuser-only"
sql_on "$DB_OWNED" -c "CREATE EXTENSION pg_automerge"
# SET before the library is loaded makes a placeholder, which Postgres
# rejects (with a WARNING) when the library defines the setting.
out="$(as_role -c "SET pg_automerge.verify_writes = off" \
    -c "SELECT '\\x'::bytea::automerge IS NOT NULL" -c "SHOW pg_automerge.verify_writes" 2>&1)"
grep -q 'permission denied to set parameter "pg_automerge.verify_writes"' <<<"$out" \
    || fail "placeholder SET not refused: $out"
grep -qx on <<<"$out" || fail "placeholder SET took effect: $out"
for stmt in "SET pg_automerge.verify_writes = off" \
            "ALTER ROLE $ROLE SET pg_automerge.verify_writes = off" \
            "ALTER DATABASE $DB_OWNED SET pg_automerge.verify_writes = off"; do
    if out="$(as_role -c "SELECT '\\x'::bytea::automerge IS NOT NULL" -c "$stmt" 2>&1)"; then
        fail "a non-superuser ran: $stmt"
    fi
    grep -q 'permission denied to set parameter "pg_automerge.verify_writes"' <<<"$out" \
        || fail "$stmt: unexpected error: $out"
done
[[ "$(as_role -c "SELECT '\\x'::bytea::automerge IS NOT NULL" -c "SHOW pg_automerge.verify_writes" | tail -1)" == on ]] || fail "verify_writes is not on for $ROLE"
# A superuser can still delegate it.
sql_on "$DB_OWNED" -c "GRANT SET ON PARAMETER pg_automerge.verify_writes TO $ROLE"
[[ "$(as_role -c "SELECT '\\x'::bytea::automerge IS NOT NULL" -c "SET pg_automerge.verify_writes = off" \
        -c "SHOW pg_automerge.verify_writes" | tail -1)" == off ]] \
    || fail "GRANT SET ON PARAMETER did not let $ROLE turn it off"
sql_on "$DB_OWNED" -c "REVOKE SET ON PARAMETER pg_automerge.verify_writes FROM $ROLE"

log "pg_automerge.max_load_memory stays superuser-only"
# The same checks: a placeholder SET is refused when the library loads,
# SET afterwards and ALTER ROLE/DATABASE .. SET fail, and it stays 2GB.
out="$(as_role -c "SET pg_automerge.max_load_memory = -1" \
    -c "SELECT '\\x'::bytea::automerge IS NOT NULL" -c "SHOW pg_automerge.max_load_memory" 2>&1)"
grep -q 'permission denied to set parameter "pg_automerge.max_load_memory"' <<<"$out" \
    || fail "placeholder SET of max_load_memory not refused: $out"
grep -qx 2GB <<<"$out" || fail "placeholder SET of max_load_memory took effect: $out"
for stmt in "SET pg_automerge.max_load_memory = -1" \
            "SET pg_automerge.max_load_memory = '64GB'" \
            "ALTER ROLE $ROLE SET pg_automerge.max_load_memory = -1" \
            "ALTER DATABASE $DB_OWNED SET pg_automerge.max_load_memory = -1"; do
    if out="$(as_role -c "SELECT '\\x'::bytea::automerge IS NOT NULL" -c "$stmt" 2>&1)"; then
        fail "a non-superuser ran: $stmt"
    fi
    grep -q 'permission denied to set parameter "pg_automerge.max_load_memory"' <<<"$out" \
        || fail "$stmt: unexpected error: $out"
done
[[ "$(as_role -c "SELECT '\\x'::bytea::automerge IS NOT NULL" -c "SHOW pg_automerge.max_load_memory" | tail -1)" \
   == 2GB ]] || fail "max_load_memory is not 2GB for $ROLE"
# A superuser can set it for a role, and delegate it.
sql_on "$DB_OWNED" -c "ALTER ROLE $ROLE SET pg_automerge.max_load_memory = '64MB'"
[[ "$(as_role -c "SELECT '\\x'::bytea::automerge IS NOT NULL" -c "SHOW pg_automerge.max_load_memory" | tail -1)" \
   == 64MB ]] || fail "ALTER ROLE .. SET by a superuser did not apply"
sql_on "$DB_OWNED" -c "ALTER ROLE $ROLE RESET pg_automerge.max_load_memory" \
    -c "GRANT SET ON PARAMETER pg_automerge.max_load_memory TO $ROLE"
[[ "$(as_role -c "SELECT '\\x'::bytea::automerge IS NOT NULL" -c "SET pg_automerge.max_load_memory = -1" \
        -c "SHOW pg_automerge.max_load_memory" | tail -1)" == -1 ]] \
    || fail "GRANT SET ON PARAMETER did not let $ROLE change max_load_memory"
sql_on "$DB_OWNED" -c "REVOKE SET ON PARAMETER pg_automerge.max_load_memory FROM $ROLE"

log "misspelled pg_automerge.* settings are removed when the library loads"
# The library reserves the prefix: a placeholder set before it loaded (here
# from ALTER DATABASE .. SET, as from postgresql.conf) is removed with a
# WARNING instead of silently shadowing nothing, and SET of a misspelled
# name fails once it is loaded.
sql_on "$DB_OWNED" -c "ALTER DATABASE $DB_OWNED SET pg_automerge.max_load_memroy = '1MB'"
if out="$(sql_on "$DB_OWNED" -c "SELECT '\\x'::bytea::automerge IS NOT NULL" \
        -c "SHOW pg_automerge.max_load_memroy" 2>&1)"; then
    fail "the misspelled setting survived loading the library: $out"
fi
grep -q 'invalid configuration parameter name "pg_automerge.max_load_memroy", removing it' <<<"$out" \
    || fail "no WARNING removing the misspelled setting: $out"
grep -q 'unrecognized configuration parameter "pg_automerge.max_load_memroy"' <<<"$out" \
    || fail "unexpected SHOW error: $out"
if out="$(sql_on "$DB_OWNED" -c "SELECT '\\x'::bytea::automerge IS NOT NULL" \
        -c "SET pg_automerge.verify_write = off" 2>&1)"; then
    fail "SET of a misspelled setting succeeded"
fi
grep -q 'invalid configuration parameter name "pg_automerge.verify_write"' <<<"$out" \
    || fail "unexpected SET error: $out"
sql_on "$DB_OWNED" -c "ALTER DATABASE $DB_OWNED RESET pg_automerge.max_load_memroy"
