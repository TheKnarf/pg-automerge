#!/usr/bin/env bash
# Build a release package of pg_automerge for the Postgres whose pg_config
# is $PG_CONFIG, and a tarball of it meant to be unpacked with
# `tar -C / -xzf`.
#
# `cargo pgrx package` lays the files out under the install paths of that
# pg_config (--pkglibdir, --sharedir/extension), so PG_CONFIG must be the
# target server's. pgrx's own development Postgres (under $PGRX_HOME) is
# refused: a package built for it would unpack into ~/.pgrx/... and
# CREATE EXTENSION on a real server would not find it.
set -euo pipefail

die() { echo "package: $*" >&2; exit 1; }

[ -n "${PG_CONFIG:-}" ] || die "set PG_CONFIG to the target server's pg_config (e.g. /usr/lib/postgresql/18/bin/pg_config)"
[ -x "$PG_CONFIG" ] || die "PG_CONFIG=$PG_CONFIG is not an executable"

pgrx_home="$(realpath -m "${PGRX_HOME:-$HOME/.pgrx}")"
pg_config_real="$(realpath -m "$PG_CONFIG")"
case "$pg_config_real/" in
  "$pgrx_home"/*) die "PG_CONFIG=$PG_CONFIG is pgrx's development Postgres (under $pgrx_home); use the target server's pg_config" ;;
esac

major="$("$PG_CONFIG" --version | sed -n 's/^PostgreSQL \([0-9]*\).*/\1/p')"
[ "$major" = "18" ] || die "PG_CONFIG=$PG_CONFIG is PostgreSQL ${major:-?}, not 18"

out=target/release/pg_automerge-pg18
rm -rf "$out"
cargo pgrx package --pg-config "$PG_CONFIG"
tar -C "$out" -czf "$out.tar.gz" .

# The tarball must unpack to where that server looks for extensions.
listing="$(tar -tzf "$out.tar.gz")"
for f in "$("$PG_CONFIG" --pkglibdir)/pg_automerge.so" \
         "$("$PG_CONFIG" --sharedir)/extension/pg_automerge.control"; do
  grep -qxF "./${f#/}" <<<"$listing" || die "$out.tar.gz lacks ./${f#/}"
done
echo "$out.tar.gz"
