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
grep -qE '^COPY .*docker/initdb-pg-automerge.sh /usr/local/bin/pg-automerge-initdb$' "$df" \
  || fail "$df: initdb script not installed as /usr/local/bin/pg-automerge-initdb (for directory mounts)"
grep -qE "^PGHOST='' PGHOSTADDR='' psql " docker/initdb-pg-automerge.sh \
  || fail "docker/initdb-pg-automerge.sh: clear PGHOST/PGHOSTADDR for psql, like the entrypoint's docker_process_sql"
if grep -qE '^(ENTRYPOINT|CMD|USER|VOLUME) ' "$df"; then fail "$df: keep the official image's entrypoint, cmd, user and volume"; fi
# Everything COPY'd from the build context is let through .dockerignore.
for src in $(grep -E '^COPY ' "$df" | grep -v -- '--from=' | sed -E 's/^COPY( --[a-z]+=[^ ]+)* //; s/ [^ ]+$//'); do
  [ "$src" = . ] && continue
  grep -qxF "!$src" .dockerignore || fail ".dockerignore does not let $src through (COPY in $df)"
done
for f in docker/initdb-pg-automerge.sh scripts/oci-source-url.sh tests/docker.sh tests/docker_lib.sh tests/docker_bench.sh; do
  bash -n "$f" || fail "$f: syntax"
done
[ -x docker/initdb-pg-automerge.sh ] || fail "docker/initdb-pg-automerge.sh must be executable (the entrypoint sources non-executable scripts)"
grep -qE '^    image: pg-automerge:' compose.yaml || fail "compose.yaml: image must be the pg-automerge built by mise run docker-build"
grep -qE '^      - pgdata:/var/lib/postgresql$' compose.yaml \
  || fail "compose.yaml: mount the volume at /var/lib/postgresql (postgres:18 keeps PGDATA in 18/docker below it)"

# The image's source label comes from the git remote through
# scripts/oci-source-url.sh: never credentials, only an https URL or nothing.
grep -qF 'source_url="$(bash scripts/oci-source-url.sh)"' scripts/docker-build.sh \
  || fail "scripts/docker-build.sh: take the source label from scripts/oci-source-url.sh"
while IFS='|' read -r remote want; do
  got=$(bash scripts/oci-source-url.sh "$remote")
  [ "$got" = "$want" ] || fail "oci-source-url.sh '$remote': expected '$want', got '$got'"
done <<'CASES'
https://x-access-token:ghp_secret@github.com/you/pg-automerge.git|https://github.com/you/pg-automerge
https://user:pa:ss@word@gitlab.example.com:8443/g/sub/repo.git/|https://gitlab.example.com:8443/g/sub/repo
https://github.com/you/repo?token=secret#x|https://github.com/you/repo
http://github.com/you/repo|https://github.com/you/repo
git@github.com:you/repo.git|https://github.com/you/repo
ssh://git@github.com:22/you/repo.git|https://github.com/you/repo
https://github.com/you/repo|https://github.com/you/repo
/home/you/pg-automerge|
file:///home/you/pg-automerge|
git@github.com:you/re po.git|
|
CASES

echo "check_ci: docker ok"

# ---------------------------------------------------------------------------
# The workflow: actionlint (syntax, expressions, runner labels, and
# running shellcheck on every run: script), shellcheck on the scripts CI
# runs, and the invariants of the docker and publish jobs.
command -v actionlint >/dev/null && command -v shellcheck >/dev/null \
  || fail "actionlint and shellcheck are needed (pinned in mise.toml: mise install)"
actionlint "$wf" || fail "$wf: actionlint"
shellcheck -x -S warning scripts/*.sh tests/check_ci.sh tests/docker.sh tests/docker_lib.sh tests/docker_bench.sh \
  docker/initdb-pg-automerge.sh || fail "shellcheck"

# job NAME: the lines of that job (from "  NAME:" to the next job).
job() { awk -v j="  $1:" '$0 == j {p=1; print; next} p && /^  [a-z]/ {exit} p' "$wf"; }
for j in ci docker publish; do [ -n "$(job "$j")" ] || fail "$wf: no $j job"; done
docker_job="$(job docker)"; publish_job="$(job publish)"
has() { grep -qE -- "$2" <<<"$1"; }

# docker: runs on every push and PR (no job-level if), amd64 always and
# arm64 on tags on the native runner, the layer cache, the tests with the
# PGDG client tools, the tag/version check and the artifact.
if has "$docker_job" '^    if:'; then fail "$wf: the docker job must run for every event"; fi
has "$docker_job" "^        arch: .*startsWith\(github\.ref, 'refs/tags/'\).*'\[\"amd64\", \"arm64\"\]'.*'\[\"amd64\"\]'" \
  || fail "$wf: docker matrix must be amd64, plus arm64 on tags"
has "$docker_job" "^    runs-on: .*'ubuntu-24.04-arm'" || fail "$wf: arm64 must build on the native arm64 runner"
has "$docker_job" 'bash scripts/docker-build.sh -- --load ' || fail "$wf: build with scripts/docker-build.sh --load"
has "$docker_job" '--cache-from "type=gha,scope=pg-automerge-\$\{\{ matrix.arch \}\}"' || fail "$wf: cache-from type=gha per arch"
has "$docker_job" '--cache-to "type=gha,scope=pg-automerge-\$\{\{ matrix.arch \}\},mode=max' || fail "$wf: cache-to type=gha,mode=max per arch"
has "$docker_job" '^        run: bash tests/docker.sh$' || fail "$wf: the docker job must run tests/docker.sh"
has "$docker_job" '^          PG_CONFIG: /usr/lib/postgresql/18/bin/pg_config$' || fail "$wf: the docker tests use PGDG's client tools"
has "$docker_job" 'DOCKER_TEST_SUITES' && fail "$wf: the docker job must run the suites"
has "$docker_job" 'GITHUB_REF_NAME" != "v\$version"' || fail "$wf: tags must be checked against the crate version"
has "$docker_job" 'bash scripts/docker-archive.sh' || fail "$wf: tags must save the image (scripts/docker-archive.sh)"

# Pushing: only the publish job, only on tags, only after both test jobs,
# and every step after the configuration check gated by it (so without
# the secret nothing is pushed). Nothing else pushes or logs in.
has "$publish_job" "^    if: startsWith\(github\.ref, 'refs/tags/'\)$" || fail "$wf: publish must be tags only"
has "$publish_job" '^    needs: \[ci, docker\]$' || fail "$wf: publish must need ci and docker"
has "$publish_job" '^          if \[ -z "\$REGISTRY_TOKEN" \]; then$' || fail "$wf: publish must check the secret"
steps=$(grep -cE '^      - ' <<<"$publish_job")
gated=$(grep -cE "^        if: steps\.cfg\.outputs\.push == 'true'$" <<<"$publish_job")
[ "$gated" -eq $((steps - 1)) ] || fail "$wf: publish: $gated of $((steps - 1)) steps after the check are gated on it"
outside="$(awk -v j="  publish:" '$0 == j {p=1} p && /^  [a-z]/ && $0 != j {p=0} !p' "$wf")"
if has "$outside" 'docker/login-action|docker-push\.sh|docker push|imagetools'; then fail "$wf: pushing outside the publish job"; fi
if grep -qE 'push: true|packages: write|write-all' "$wf"; then fail "$wf: no push: true or write permissions"; fi
grep -qE '^permissions:$' "$wf" && grep -qE '^  contents: read$' "$wf" || fail "$wf: top-level permissions must be contents: read"

echo "check_ci: workflow ok"
