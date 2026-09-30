#!/usr/bin/env bash
# Benchmark of the everyday SQL paths, each statement timed on its own by
# psql (simple query protocol, `\timing`), writes inside BEGIN .. ROLLBACK:
#
#   R1  SELECT doc->>'status'                     one jsonb accessor
#   R2  three accessors in one SELECT
#   R3  automerge_heads(doc)
#   I1  INSERT of a newer compressed full save (bytea, assignment cast)
#   W1  UPDATE .. SET doc = merge(doc, changes::bytea)   incremental changes
#   W2  UPDATE .. SET doc = merge(doc, save::bytea)      newer full save
#   W3  UPDATE .. SET doc = merge(doc, save::automerge)  the same, cast first
#   W4  INSERT .. ON CONFLICT DO UPDATE SET doc = merge(docs.doc, excluded.doc)
#   W5  W1 again after W1 (the changes are already there: a no-op)
#   W6  W3 with the save as a text parameter (extended protocol, \bind)
#   A1  merge_agg over one row
#   A2  merge_agg over the row and a newer version of it (stored rows)
#   C1  automerge_contains(doc, newer save bytea)   (false)
#   C2  automerge_contains(doc, newer stored version)   (false)
#
# "newer" is the base document loaded from its save, one change by another
# actor, saved compressed (Automerge.save()); "changes" is save_after(base
# heads) of it (see the gen_bench example). Prints the median of
# BENCH_REPS runs (default 3) in milliseconds per case and document.
# Run via `mise run bench-sql`. Installs a release build into the
# pgrx-managed pg18 and uses database pg_automerge_bench_sql. Not part of
# `mise run test`. Against an external server (tests/lib.sh), its own
# build is timed: `mise run docker-bench-sql` runs this against the Docker
# image (tests/docker_bench.sh).
#
# Env: see tests/lib.sh; also BENCH_DOCS (default "items20k text3mb"; also
# items2k, rich20k), BENCH_REPS, BENCH_CASES (a grep -E pattern on the case
# ids, default all), BENCH_REUSE=1 (keep the database of a previous run),
# BENCH_NO_INSTALL=1 (use the installed build as is), BENCH_SETTINGS (SQL
# run first in every session, e.g. "SET pg_automerge.verify_writes = off").

# shellcheck source=tests/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

DB=pg_automerge_bench_sql
DOCS="${BENCH_DOCS:-items20k text3mb}"
REPS="${BENCH_REPS:-3}"
CASE_FILTER="${BENCH_CASES:-.}"

# Progress goes to stderr: stdout is the results table.
log() { printf '==> %s\n' "$*" >&2; }

[[ "${BENCH_NO_INSTALL:-0}" == 1 ]] || install_extension --release
start_server

if [[ "${BENCH_REUSE:-0}" == 1 ]] && [[ "$(sql_on postgres -c "SELECT count(*) FROM pg_database WHERE datname = '$DB'")" == 1 ]]; then
    log "reusing database $DB"
else
    log "generating fixtures"
    cargo run --release -q -p pg_automerge_core --example gen_bench >"$WORK/fixtures.sql"
    create_db
    sql -c "CREATE EXTENSION pg_automerge"
    log "loading fixtures"
    sql -f "$WORK/fixtures.sql" >/dev/null
    sql -c "CREATE TABLE docs (name text PRIMARY KEY, doc automerge NOT NULL)" \
        -c "INSERT INTO docs SELECT name, doc FROM bench_doc" -c "VACUUM ANALYZE"
fi

NEWER="(SELECT save FROM bench_newer WHERE name = :'d')"
CHANGES="(SELECT changes FROM bench_newer WHERE name = :'d')"
IDS=(); SETUPS=(); STMTS=()
# case <id> <setup (untimed, inside the transaction)> <timed statement>
case_() { IDS+=("$1"); SETUPS+=("$2"); STMTS+=("$3"); }
case_ R1 "" "SELECT doc->>'status' FROM docs WHERE name = :'d';"
case_ R2 "" "SELECT doc->>'status', doc->'a', doc->'b' FROM docs WHERE name = :'d';"
case_ R3 "" "SELECT cardinality(automerge_heads(doc)) FROM docs WHERE name = :'d';"
case_ I1 "" "INSERT INTO docs VALUES (:'d' || 'x', $NEWER);"
case_ W1 "" "UPDATE docs SET doc = merge(doc, $CHANGES) WHERE name = :'d';"
case_ W2 "" "UPDATE docs SET doc = merge(doc, $NEWER) WHERE name = :'d';"
case_ W3 "" "UPDATE docs SET doc = merge(doc, $NEWER::automerge) WHERE name = :'d';"
case_ W4 "" "INSERT INTO docs VALUES (:'d', $NEWER) ON CONFLICT (name) DO UPDATE SET doc = merge(docs.doc, excluded.doc);"
case_ W5 "UPDATE docs SET doc = merge(doc, $CHANGES) WHERE name = :'d';" \
    "UPDATE docs SET doc = merge(doc, $CHANGES) WHERE name = :'d';"
case_ W6 "SELECT '\\x' || encode(save, 'hex') AS newer_hex FROM bench_newer WHERE name = :'d' \\gset" \
    "UPDATE docs SET doc = merge(doc, \$1::automerge) WHERE name = :'d' \\bind :newer_hex \\g"
case_ A1 "" "SELECT cardinality(automerge_heads(merge_agg(doc))) FROM docs WHERE name = :'d';"
case_ A2 "INSERT INTO docs VALUES (:'d' || 'n', $NEWER);" \
    "SELECT cardinality(automerge_heads(merge_agg(doc ORDER BY name))) FROM docs WHERE name IN (:'d', :'d' || 'n');"
case_ C1 "" "SELECT automerge_contains(doc, $NEWER) FROM docs WHERE name = :'d';"
case_ C2 "INSERT INTO docs VALUES (:'d' || 'n', $NEWER);" \
    "SELECT automerge_contains(a.doc, b.doc) FROM docs a, docs b WHERE a.name = :'d' AND b.name = :'d' || 'n';"

# One psql session per case and document: REPS transactions, each timing
# only the case's statement. Prints the times (ms), one per line.
run_case() {
    local doc="$1" setup="$2" stmt="$3" script="$WORK/case.sql"
    {
        # Terminated here, so a settings string without its own ';' works
        # (an extra empty statement is harmless).
        [[ -z "${BENCH_SETTINGS:-}" ]] || printf '%s;\n' "$BENCH_SETTINGS"
        for _ in $(seq 1 "$REPS"); do
            printf 'BEGIN;\n%s\n\\timing on\n%s\n\\timing off\nROLLBACK;\n' "$setup" "$stmt"
        done
    } >"$script"
    PGOPTIONS="-c client_min_messages=warning" "$BINDIR/psql" -X -q -v ON_ERROR_STOP=1 \
        "${CONN[@]}" -d "$DB" -v d="$doc" -f "$script" \
        | sed -n 's/^Time: \([0-9.]*\) ms.*/\1/p'
}

median() { sort -n | awk '{ v[NR] = $1 } END { if (NR % 2) print v[(NR + 1) / 2]; else printf "%.3f\n", (v[NR / 2] + v[NR / 2 + 1]) / 2 }'; }

header="| case |"; rule="|---|"
for doc in $DOCS; do header+=" $doc |"; rule+="---:|"; done
echo "$header"; echo "$rule"
for i in "${!IDS[@]}"; do
    id="${IDS[i]}"; setup="${SETUPS[i]}"; stmt="${STMTS[i]}"
    grep -qE "$CASE_FILTER" <<<"$id" || continue
    row="| $id |"
    for doc in $DOCS; do
        log "$id $doc"
        times="$(run_case "$doc" "$setup" "$stmt")"
        [[ "$(wc -l <<<"$times")" == "$REPS" ]] || fail "$id $doc: expected $REPS timings, got: $times"
        log "  $(tr '\n' ' ' <<<"$times")"
        row+=" $(median <<<"$times" | awk '{ printf ($1 < 10 ? "%.1f" : "%.0f"), $1 }') |"
    done
    echo "$row"
done
