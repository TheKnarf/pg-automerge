#!/usr/bin/env bash
# Check that the fixture documents in tests/pg_regress/sql/automerge.sql
# (its `\set name '\\x...'` lines) are exactly what
# crates/pg_automerge_core/examples/gen_regress.rs prints (run by
# `mise run regress`). To update them, replace those lines with the output
# of `cargo run -p pg_automerge_core --example gen_regress`.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

SQL=tests/pg_regress/sql/automerge.sql
if ! diff -u \
    <(grep -E "^\\\\set [a-z_]+ '" "$SQL") \
    <(cargo run -q -p pg_automerge_core --example gen_regress) >&2; then
    echo "FAIL: the fixtures in $SQL differ from gen_regress output (- file, + generator)" >&2
    exit 1
fi
echo "==> regress fixtures match gen_regress"
