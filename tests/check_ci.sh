#!/usr/bin/env bash
# Static checks of the CI workflow, the package task and the Docker
# packaging (part of `mise run lint`). Nothing here builds anything: the
# package script must refuse a bad PG_CONFIG before it starts cargo.
set -euo pipefail
cd "$(dirname "$0")/.."

fail() { echo "check_ci: $*" >&2; exit 1; }
wf=.github/workflows/ci.yml

# The concurrency group must separate events, so a push to main does not
# cancel the nightly (schedule) run on the same ref, or vice versa.
grep -qE '^  group: .*\$\{\{ github\.event_name \}\}.*\$\{\{ github\.ref \}\}' "$wf" \
  || fail "$wf: concurrency group must include github.event_name and github.ref"

# The tag build must package against the distro/PGDG Postgres, not pgrx's.
grep -qE '^        run: PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config mise run package$' "$wf" \
  || fail "$wf: the package step must set PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config"
if grep -E 'mise run package' "$wf" | grep -vq 'PG_CONFIG=/usr/lib/postgresql/'; then
  fail "$wf: every 'mise run package' must set a distro PG_CONFIG"
fi

# scripts/package.sh refuses a missing PG_CONFIG and pgrx's own Postgres
# (directly or through a symlink), before building anything.
expect_refusal() { # <pattern> <env...>
  local pattern=$1 out; shift
  if out=$(env "$@" bash scripts/package.sh 2>&1); then
    fail "package.sh accepted: $* (output: $out)"
  fi
  grep -q "$pattern" <<<"$out" || fail "package.sh with $*: expected '$pattern', got: $out"
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fake_pgrx="$tmp/pgrx"
mkdir -p "$fake_pgrx/18.6/pgrx-install/bin"
cat >"$fake_pgrx/18.6/pgrx-install/bin/pg_config" <<'SH'
#!/bin/sh
echo "PostgreSQL 18.6"
SH
chmod +x "$fake_pgrx/18.6/pgrx-install/bin/pg_config"
ln -s "$fake_pgrx/18.6/pgrx-install/bin/pg_config" "$tmp/pg_config_link"
mkdir -p "$tmp/pg17/bin"
printf '#!/bin/sh\necho "PostgreSQL 17.2"\n' >"$tmp/pg17/bin/pg_config"
chmod +x "$tmp/pg17/bin/pg_config"

expect_refusal "set PG_CONFIG" -u PG_CONFIG
expect_refusal "not an executable" PG_CONFIG="$tmp/missing"
expect_refusal "development Postgres" PGRX_HOME="$fake_pgrx" PG_CONFIG="$fake_pgrx/18.6/pgrx-install/bin/pg_config"
expect_refusal "development Postgres" PGRX_HOME="$fake_pgrx" PG_CONFIG="$tmp/pg_config_link"
expect_refusal "not 18" PGRX_HOME="$fake_pgrx" PG_CONFIG="$tmp/pg17/bin/pg_config"

# The real pgrx Postgres, if initialized, is refused too.
real=$(sed -n 's/^pg18 *= *"\(.*\)"/\1/p' "${PGRX_HOME:-$HOME/.pgrx}/config.toml" 2>/dev/null || true)
if [ -n "$real" ] && [ -x "$real" ]; then
  expect_refusal "development Postgres" PG_CONFIG="$real"
fi

echo "check_ci: ok"

# Docker image (tests/docker.sh builds and runs it; these checks need no
# Docker). The pinned versions come from scripts/versions.sh only.
df=docker/Dockerfile
versions="$(sh scripts/versions.sh)" || fail "scripts/versions.sh failed"
for v in RUST_VERSION CARGO_PGRX_VERSION CRATE_VERSION; do
  grep -qE "^$v=[0-9]+\.[0-9]+\.[0-9]+$" <<<"$versions" || fail "scripts/versions.sh: no $v in: $versions"
done
for v in $(sed -n 's/^[A-Z_]*=//p' <<<"$versions" | sort -u); do
  if grep -vE '^\s*#' "$df" | grep -qF "$v"; then fail "$df repeats the version $v (read it with scripts/versions.sh)"; fi
done
# One pinned base for every stage, so the builder's glibc and Postgres
# headers are the runtime's.
grep -qE '^ARG PG_IMAGE=postgres:18-[a-z]+@sha256:[0-9a-f]{64}$' "$df" \
  || fail "$df: PG_IMAGE must be postgres:18-<debian>@sha256:<digest>"
if grep -E '^FROM ' "$df" | grep -vqE '^FROM \$\{PG_IMAGE\} AS [a-z]+$'; then
  fail "$df: every stage must be FROM \${PG_IMAGE}"
fi
grep -qE '^COPY .*docker/initdb-pg-automerge.sh /docker-entrypoint-initdb.d/' "$df" || fail "$df: initdb script not installed"
if grep -qE '^(ENTRYPOINT|CMD|USER|VOLUME) ' "$df"; then fail "$df: keep the official image's entrypoint, cmd, user and volume"; fi
# Everything COPY'd from the build context is let through .dockerignore.
for src in $(grep -E '^COPY ' "$df" | grep -v -- '--from=' | sed -E 's/^COPY( --[a-z]+=[^ ]+)* //; s/ [^ ]+$//'); do
  [ "$src" = . ] && continue
  grep -qxF "!$src" .dockerignore || fail ".dockerignore does not let $src through (COPY in $df)"
done
grep -qE '^        run: mise run docker-test$' "$wf" || fail "$wf: CI must run mise run docker-test"
for f in docker/initdb-pg-automerge.sh tests/docker.sh tests/docker_lib.sh tests/docker_bench.sh; do
  bash -n "$f" || fail "$f: syntax"
done
[ -x docker/initdb-pg-automerge.sh ] || fail "docker/initdb-pg-automerge.sh must be executable (the entrypoint sources non-executable scripts)"
grep -qE '^    image: pg-automerge:' compose.yaml || fail "compose.yaml: image must be the pg-automerge built by mise run docker-build"
grep -qE '^      - pgdata:/var/lib/postgresql$' compose.yaml \
  || fail "compose.yaml: mount the volume at /var/lib/postgresql (postgres:18 keeps PGDATA in 18/docker below it)"

echo "check_ci: docker ok"
