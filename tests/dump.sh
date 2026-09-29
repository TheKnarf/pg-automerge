#!/usr/bin/env bash
# pg_dump / pg_restore and COPY round trips of every kind of object that
# depends on the automerge type (run via `mise run dump`, also part of
# `mise run test`).
#
# The source database has the extension in its own schema (CREATE
# EXTENSION .. SCHEMA ext) that is not on the search_path of the restore,
# and a table with: automerge columns (small, compressed-input and TOASTed
# documents), a STORED generated column doc::jsonb, a GIN and a btree
# expression index on doc::jsonb, an automerge_notify() trigger, a check
# constraint using an extension function, and views using merge, the ||
# operator and merge_agg.
#
# Checked for a plain dump restored with psql and a custom-format dump
# restored with pg_restore --exit-on-error: the fingerprint of every row
# (md5 of the stored bytes, heads, jsonb, generated column) and of the
# views; the indexes are valid and used; the trigger fires (a LISTEN
# session receives its notification). Also a binary COPY TO / FROM round
# trip, and that COPY FROM rejects a corrupt value with 22P02. The
# custom-format restore runs with pg_automerge.verify_writes = off (set on
# its database) and must give the same bytes; the dump does not mention
# the setting.
# A restore into a database with a lower pg_automerge.max_load_memory
# fails with 53400, and succeeds with the limit raised for the restore
# session (PGOPTIONS='-c pg_automerge.max_load_memory=-1').
#
# Env: see tests/lib.sh.

# shellcheck source=tests/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

DB=pg_automerge_dump
DB_PLAIN=pg_automerge_dump_plain
DB_CUSTOM=pg_automerge_dump_custom
DB_LOW=pg_automerge_dump_low
DROP_DBS=("$DB" "$DB_PLAIN" "$DB_CUSTOM" "$DB_LOW")

on_exit() {
    if [[ $1 == 0 ]]; then log "all dump/restore checks passed"; else echo "dump test FAILED" >&2; fi
}

log "generating fixture documents"
eval "$(cargo run -q -p pg_automerge_core --example gen_concurrency)"

install_extension
start_server
create_db

# The restores run with a search_path without the extension's schema:
# every reference to it must be qualified in the dump.
APP_PATH="SET search_path = app;"

sql -v base="$BASE" -v a="$NEW_A" -v b="$NEW_B" -v inc_a="$INC_A" -v many="$MANY" <<'SQL'
CREATE SCHEMA ext;
CREATE EXTENSION pg_automerge SCHEMA ext;
CREATE SCHEMA app;
SET search_path = app, ext;
CREATE TABLE docs (
    id int PRIMARY KEY,
    doc automerge NOT NULL CHECK (cardinality(automerge_heads(doc)) >= 1),
    data jsonb GENERATED ALWAYS AS (doc::jsonb) STORED
);
CREATE INDEX docs_data_gin ON docs USING gin ((doc::jsonb) jsonb_path_ops);
CREATE INDEX docs_title ON docs ((doc->>'title'));
CREATE TRIGGER docs_notify AFTER INSERT OR UPDATE OR DELETE ON docs
    FOR EACH ROW EXECUTE FUNCTION automerge_notify('docs_changed', 'id');
INSERT INTO docs (id, doc) VALUES
    (1, :'base'),                               -- compressed save
    (2, :'a'::bytea::automerge),
    (3, merge(:'a'::automerge, :'b'::automerge)),
    (4, :'base'::automerge || :'inc_a'::bytea),
    (5, :'many');                               -- 150 heads, TOASTed
CREATE VIEW merged AS
    SELECT x.id, merge(x.doc, y.doc) AS m, x.doc || y.doc AS o
    FROM docs x JOIN docs y ON y.id = x.id + 1;
CREATE VIEW all_merged AS SELECT merge_agg(doc ORDER BY id) AS m FROM docs;
SQL

[[ "$(sql -c "SELECT pg_column_toast_chunk_id(doc) IS NOT NULL FROM app.docs WHERE id = 5")" == t ]] \
    || fail "document 5 should be stored out of line (TOAST)"

fingerprint() {
    sql_on "$1" -c "$APP_PATH
        SELECT string_agg(id || ':' || md5(doc::bytea) || ':' || ext.automerge_heads(doc)::text
                          || ':' || md5(doc::jsonb::text) || ':' || md5(data::text), ',' ORDER BY id)
        FROM docs;
        SELECT string_agg(id || ':' || md5(m::bytea) || ':' || md5(o::jsonb::text), ',' ORDER BY id)
        FROM merged;
        SELECT md5(m::bytea) FROM all_merged;"
}

check_restored() {
    local db="$1" what="$2"
    local got
    got="$(fingerprint "$db")"
    [[ "$got" == "$EXPECTED" ]] || fail "$what: restored data differs:
$got
expected:
$EXPECTED"
    [[ "$(sql_on "$db" -c "SELECT extnamespace::regnamespace FROM pg_extension WHERE extname = 'pg_automerge'")" == ext ]] \
        || fail "$what: extension not in schema ext"
    [[ "$(sql_on "$db" -c "SELECT count(*) FROM pg_index WHERE indrelid = 'app.docs'::regclass AND indisvalid")" == 3 ]] \
        || fail "$what: indexes missing or invalid"
    local plan
    plan="$(sql_on "$db" -c "$APP_PATH SET enable_seqscan = off;
        EXPLAIN (COSTS OFF) SELECT id FROM docs WHERE doc::jsonb @> '{\"base\": true}'")"
    grep -q docs_data_gin <<<"$plan" || fail "$what: GIN expression index not used: $plan"
    [[ "$(sql_on "$db" -c "$APP_PATH SET enable_seqscan = off;
        SELECT string_agg(id::text, ',' ORDER BY id) FROM docs WHERE doc::jsonb @> '{\"base\": true}'")" \
       == "$(sql -c "SET search_path = app; SELECT string_agg(id::text, ',' ORDER BY id) FROM docs WHERE doc::jsonb @> '{\"base\": true}'")" ]] \
        || fail "$what: index scan result differs"
    # The trigger fires: one session listens, writes, and receives it.
    local out
    out="$(sql_on "$db" -v inc="$INC_B" <<SQL 2>&1
$APP_PATH
LISTEN docs_changed;
UPDATE docs SET doc = doc OPERATOR(ext.||) :'inc'::bytea WHERE id = 1;
SELECT 1;
SQL
)"
    grep -q 'Asynchronous notification "docs_changed" with payload "{"table":"app.docs","op":"UPDATE",.*"key":{"id":1}' <<<"$out" \
        || fail "$what: the trigger did not notify: $out"
    # The generated column followed the update.
    [[ "$(sql_on "$db" -c "$APP_PATH SELECT data = doc::jsonb AND data->>'inc_b' IS NOT NULL FROM docs WHERE id = 1")" == t ]] \
        || fail "$what: generated column not recomputed"
}

EXPECTED="$(fingerprint "$DB")"

log "plain pg_dump restored with psql"
sql_on postgres -c "DROP DATABASE IF EXISTS $DB_PLAIN WITH (FORCE)" -c "CREATE DATABASE $DB_PLAIN"
"$BINDIR/pg_dump" -h localhost -p "$PORT" -d "$DB" >"$WORK/dump.sql"
grep -q "CREATE EXTENSION IF NOT EXISTS pg_automerge WITH SCHEMA ext" "$WORK/dump.sql" \
    || fail "dump lacks CREATE EXTENSION .. WITH SCHEMA ext"
sql_on "$DB_PLAIN" -f "$WORK/dump.sql" >/dev/null
check_restored "$DB_PLAIN" "plain dump"

log "custom-format pg_dump restored with pg_restore --exit-on-error"
sql_on postgres -c "DROP DATABASE IF EXISTS $DB_CUSTOM WITH (FORCE)" -c "CREATE DATABASE $DB_CUSTOM"
# Restored with pg_automerge.verify_writes off: the setting only decides
# whether a check can reject input, so the restored bytes are the same.
sql_on postgres -c "ALTER DATABASE $DB_CUSTOM SET pg_automerge.verify_writes = off"
"$BINDIR/pg_dump" -h localhost -p "$PORT" -d "$DB" -Fc -f "$WORK/dump.custom"
"$BINDIR/pg_restore" -h localhost -p "$PORT" -d "$DB_CUSTOM" --exit-on-error "$WORK/dump.custom" \
    || fail "pg_restore failed"
check_restored "$DB_CUSTOM" "custom dump"
[[ "$(sql_on "$DB_CUSTOM" -c "SELECT current_setting('pg_automerge.verify_writes')")" == off ]] \
    || fail "custom dump: restore database should run with pg_automerge.verify_writes off"
! grep -q verify_writes "$WORK/dump.sql" || fail "the dump mentions pg_automerge.verify_writes"

log "restore into a database with a lower pg_automerge.max_load_memory"
# Every value restored is new input to the restoring server: under a
# limit below what the documents take to load, the restore fails with
# 53400; raising the limit for the restore session, as DESIGN.md
# documents, restores them unchanged. The dump does not mention it.
sql_on postgres -c "DROP DATABASE IF EXISTS $DB_LOW WITH (FORCE)" -c "CREATE DATABASE $DB_LOW" \
    -c "ALTER DATABASE $DB_LOW SET pg_automerge.max_load_memory = '64kB'"
if out="$("$BINDIR/pg_restore" -h localhost -p "$PORT" -d "$DB_LOW" --exit-on-error "$WORK/dump.custom" 2>&1)"; then
    fail "restore under a 64kB limit succeeded"
fi
grep -q 'estimated memory to load automerge input exceeds "pg_automerge.max_load_memory" (64 kB)' <<<"$out" \
    || fail "unexpected restore error: $out"
sql_on postgres -c "DROP DATABASE $DB_LOW WITH (FORCE)" -c "CREATE DATABASE $DB_LOW" \
    -c "ALTER DATABASE $DB_LOW SET pg_automerge.max_load_memory = '64kB'"
PGOPTIONS='-c pg_automerge.max_load_memory=-1' "$BINDIR/pg_restore" -h localhost -p "$PORT" -d "$DB_LOW" \
    --exit-on-error "$WORK/dump.custom" || fail "restore with the limit raised failed"
[[ "$(sql_on "$DB_LOW" -c "LOAD 'pg_automerge'" \
        -c "SELECT current_setting('pg_automerge.max_load_memory')")" == 64kB ]] \
    || fail "the database's own limit changed"
# (check_restored merges: with the database's limit back to the default.)
sql_on postgres -c "ALTER DATABASE $DB_LOW RESET pg_automerge.max_load_memory"
check_restored "$DB_LOW" "restore with the limit raised"
! grep -q max_load_memory "$WORK/dump.sql" || fail "the dump mentions pg_automerge.max_load_memory"

log "binary COPY round trip"
sql -c "\\copy app.docs (id, doc) TO '$WORK/docs.bin' (FORMAT binary)"
sql <<'SQL'
CREATE TABLE app.docs_copy (id int PRIMARY KEY, doc ext.automerge NOT NULL);
SQL
sql -c "\\copy app.docs_copy (id, doc) FROM '$WORK/docs.bin' (FORMAT binary)"
[[ "$(sql -c "SELECT count(*) FROM app.docs d JOIN app.docs_copy c USING (id) WHERE d.doc::bytea = c.doc::bytea")" == 5 ]] \
    || fail "binary COPY changed values"
sql -c "\\copy app.docs (id, doc) TO '$WORK/docs.txt'"
sql -c "TRUNCATE app.docs_copy"
sql -c "\\copy app.docs_copy (id, doc) FROM '$WORK/docs.txt'"
[[ "$(sql -c "SELECT count(*) FROM app.docs d JOIN app.docs_copy c USING (id) WHERE d.doc::bytea = c.doc::bytea")" == 5 ]] \
    || fail "text COPY changed values"
# COPY FROM validates every value: a corrupt one fails the COPY with 22P02.
printf '9\t\\\\x856f4a83deadbeef\n' >"$WORK/bad.txt"
if out="$(sql -c "\\copy app.docs_copy (id, doc) FROM '$WORK/bad.txt'" 2>&1)"; then
    fail "COPY FROM accepted a corrupt value"
fi
grep -q "invalid automerge document" <<<"$out" || fail "unexpected COPY error: $out"
