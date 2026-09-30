#!/bin/sh
# Print the pinned versions as shell assignments, read from the repository's
# own files so they are never repeated by hand:
#   RUST_VERSION        mise.toml [tools] rust
#   CARGO_PGRX_VERSION  mise.toml [tools] "cargo:cargo-pgrx" (must equal the
#                       pgrx crate in Cargo.lock)
#   CRATE_VERSION       Cargo.toml [package] version (the extension version)
# Used by docker/Dockerfile (versions stage), scripts/docker-build.sh and
# tests/check_ci.sh. POSIX sh: it runs in the postgres base image too.
#
# Usage: versions.sh [repository dir, default: this script's parent]
set -eu
dir="${1:-$(dirname "$0")/..}"
fail() { echo "versions.sh: $*" >&2; exit 1; }

rust=$(sed -n 's/^rust *= *{ *version *= *"\([^"]*\)".*/\1/p' "$dir/mise.toml")
pgrx_cli=$(sed -n 's/^"cargo:cargo-pgrx" *= *"\([^"]*\)".*/\1/p' "$dir/mise.toml")
pgrx_lib=$(awk '/^\[\[package\]\]/{p=0} /^name = "pgrx"$/{p=1} p && /^version = /{gsub(/"/,"",$3); print $3; exit}' "$dir/Cargo.lock")
crate=$(awk '/^\[package\]/{p=1; next} /^\[/{p=0} p && /^version = /{gsub(/"/,"",$3); print $3; exit}' "$dir/Cargo.toml")

[ -n "$rust" ] || fail "no rust version in mise.toml"
[ -n "$pgrx_cli" ] || fail "no cargo-pgrx version in mise.toml"
[ -n "$pgrx_lib" ] || fail "no pgrx package in Cargo.lock"
[ "$pgrx_cli" = "$pgrx_lib" ] || fail "cargo-pgrx $pgrx_cli (mise.toml) != pgrx $pgrx_lib (Cargo.lock)"
[ -n "$crate" ] || fail "no [package] version in Cargo.toml"

printf 'RUST_VERSION=%s\nCARGO_PGRX_VERSION=%s\nCRATE_VERSION=%s\n' "$rust" "$pgrx_cli" "$crate"
