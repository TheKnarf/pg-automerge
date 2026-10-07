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

# The release build must package against the distro/PGDG Postgres, not pgrx's.
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
# headers are the runtime's; the one exception is the CNPG extension image,
# FROM scratch (files only).
grep -qE '^ARG PG_IMAGE=postgres:18-[a-z]+@sha256:[0-9a-f]{64}$' "$df" \
  || fail "$df: PG_IMAGE must be postgres:18-<debian>@sha256:<digest>"
if grep -E '^FROM ' "$df" | grep -vxE 'FROM scratch AS cnpg-extension' | grep -vqE '^FROM \$\{PG_IMAGE\} AS [a-z-]+$'; then
  fail "$df: every stage but cnpg-extension (FROM scratch) must be FROM \${PG_IMAGE}"
fi
# The full image stays the default target: runtime is the last stage.
[ "$(grep -E '^FROM ' "$df" | tail -1)" = 'FROM ${PG_IMAGE} AS runtime' ] || fail "$df: runtime must be the last (default) stage"
# stage NAME: the lines of that stage, from its FROM to the next one.
stage() { awk -v s="$1" '/^FROM / {p = ($NF == s)} p' "$df"; }
runtime_stage="$(stage runtime)"; cnpg_stage="$(stage cnpg-extension)"; cnpg_files="$(stage cnpg-files)"
[ -n "$cnpg_stage" ] && [ -n "$cnpg_files" ] || fail "$df: no cnpg-extension or cnpg-files stage"
# The CNPG image: CNPG's layout (/lib, /share/extension) and the license,
# nothing else: one COPY of the tree cnpg-files assembled and checked
# (only the builder's /out and LICENSE go into it), its labels, and the
# non-root user of CNPG's own extension images.
[ "$(grep -E '^(COPY|ADD|RUN) ' <<<"$cnpg_stage")" = 'COPY --from=cnpg-files /cnpg/ /' ] \
  || fail "$df: cnpg-extension must hold only COPY --from=cnpg-files /cnpg/ /"
grep -qxF 'USER 65532:65532' <<<"$cnpg_stage" || fail "$df: cnpg-extension must end with USER 65532:65532"
for l in org.opencontainers.image.version io.cloudnativepg.image.base.pgmajor io.cloudnativepg.image.base.os io.cloudnativepg.image.sql.version; do
  grep -qF "$l=" <<<"$cnpg_stage" || fail "$df: cnpg-extension has no $l label"
done
[ "$(grep -E '^COPY ' <<<"$cnpg_files" | sed -E 's/ +/ /g')" = "$(printf '%s\n' \
  'COPY --from=builder /out/lib/pg_automerge.so /cnpg/lib/' \
  'COPY --from=builder /out/extension/ /cnpg/share/extension/' \
  'COPY LICENSE /cnpg/licenses/pg_automerge/copyright')" ] || fail "$df: cnpg-files copies more or other than the library, extension files and license"
grep -qF 'PG_DEBIAN" = "$VERSION_CODENAME"' <<<"$cnpg_files" || fail "$df: cnpg-files must check PG_DEBIAN against the base's Debian release"
# The smoke test runs a digest-pinned CNPG operand image.
grep -qE '^    \[trixie\]=ghcr\.io/cloudnative-pg/postgresql:18-minimal-trixie@sha256:[0-9a-f]{64}$' tests/cnpg_smoke.sh \
  || fail "tests/cnpg_smoke.sh: pin the CNPG operand image by digest"
for guc in "extension_control_path = '\\\$system:\$MOUNT/share'" "dynamic_library_path = '\\\$libdir:\$MOUNT/lib'"; do
  grep -qxF "$guc" tests/cnpg_smoke.sh || fail "tests/cnpg_smoke.sh: set $guc, as CNPG does"
done
grep -qE '^MOUNT=/extensions/pg-automerge ' tests/cnpg_smoke.sh || fail "tests/cnpg_smoke.sh: mount at /extensions/pg-automerge, as CNPG does"
grep -qF 'bash tests/cnpg_smoke.sh' tests/docker.sh || fail "tests/docker.sh must run tests/cnpg_smoke.sh"
for t in docker-build-cnpg cnpg-smoke cnpg-e2e; do grep -qxF "[tasks.$t]" mise.toml || fail "mise.toml: no $t task"; done
# The end-to-end test: the smoke test's operand, a digest-pinned kind node,
# the operator's manifest checked by sha256, a kubeconfig of its own (never
# the user's cluster), the images loaded into the node and never pulled;
# kind and kubectl pinned; not part of ci (too heavy).
smoke_operand="$(sed -nE 's/^    \[trixie\]=(.*)$/\1/p' tests/cnpg_smoke.sh)"
[ -n "$smoke_operand" ] && grep -qxF "OPERAND=$smoke_operand" tests/cnpg_e2e.sh \
  || fail "tests/cnpg_e2e.sh: OPERAND must be tests/cnpg_smoke.sh's trixie operand"
grep -qE '^NODE_IMAGE="\$\{CNPG_E2E_NODE_IMAGE:-kindest/node:v[0-9.]+@sha256:[0-9a-f]{64}\}"$' tests/cnpg_e2e.sh \
  || fail "tests/cnpg_e2e.sh: pin the kind node image by digest"
grep -qE '^CNPG_MANIFEST_SHA256=[0-9a-f]{64}$' tests/cnpg_e2e.sh && grep -qF 'sha256sum -c' tests/cnpg_e2e.sh \
  || fail "tests/cnpg_e2e.sh: check the operator manifest's sha256"
grep -qxF 'export KUBECONFIG="$DWORK/kubeconfig"   # never the user'"'"'s own' tests/cnpg_e2e.sh \
  || fail "tests/cnpg_e2e.sh: use a kubeconfig of its own"
[ "$(grep -c '          pullPolicy: Never' tests/cnpg_e2e.sh)" = 1 ] || fail "tests/cnpg_e2e.sh: extension images pullPolicy: Never"
for tool in '"aqua:kubernetes-sigs/kind" = "' 'kubectl = "'; do
  grep -qE "^${tool}[0-9.]+\"$" mise.toml || fail "mise.toml: pin $tool"
done
if sed -n '/^\[tasks.ci\]/,/^\[/p' mise.toml | grep -q cnpg-e2e || grep -qF cnpg_e2e tests/docker.sh; then
  fail "cnpg-e2e must not be part of mise run ci or docker-test"
fi
grep -qE '^COPY .*docker/initdb-pg-automerge.sh /docker-entrypoint-initdb.d/' "$df" || fail "$df: initdb script not installed"
grep -qE '^COPY .*docker/initdb-pg-automerge.sh /usr/local/bin/pg-automerge-initdb$' "$df" \
  || fail "$df: initdb script not installed as /usr/local/bin/pg-automerge-initdb (for directory mounts)"
grep -qE "^PGHOST='' PGHOSTADDR='' psql " docker/initdb-pg-automerge.sh \
  || fail "docker/initdb-pg-automerge.sh: clear PGHOST/PGHOSTADDR for psql, like the entrypoint's docker_process_sql"
if grep -qE '^(ENTRYPOINT|CMD|VOLUME) ' "$df" || grep -qE '^USER ' <<<"$runtime_stage"; then
  fail "$df: keep the official image's entrypoint, cmd, user and volume"
fi
# Everything COPY'd from the build context is let through .dockerignore.
for src in $(grep -E '^COPY ' "$df" | grep -v -- '--from=' | sed -E 's/^COPY( --[a-z]+=[^ ]+)* //; s/ [^ ]+$//'); do
  [ "$src" = . ] && continue
  grep -qxF "!$src" .dockerignore || fail ".dockerignore does not let $src through (COPY in $df)"
done
for f in docker/initdb-pg-automerge.sh scripts/oci-source-url.sh tests/docker.sh tests/docker_lib.sh tests/docker_bench.sh tests/cnpg_smoke.sh tests/cnpg_e2e.sh; do
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
# The workflows: actionlint (syntax, expressions, runner labels, and
# running shellcheck on every run: script), shellcheck on the scripts CI
# runs, and the invariants of ci.yml's jobs and of release.yml, the only
# workflow that publishes.
command -v actionlint >/dev/null && command -v shellcheck >/dev/null \
  || fail "actionlint and shellcheck are needed (pinned in mise.toml: mise install)"
docs_wf=.github/workflows/deploy-docs.yml
rel=.github/workflows/release.yml
[ -f "$rel" ] || fail "$rel missing (releases are published by it)"
actionlint "$wf" "$rel" "$docs_wf" || fail "$wf, $rel, $docs_wf: actionlint"
shellcheck -x -S warning scripts/*.sh tests/check_ci.sh tests/docker.sh tests/docker_lib.sh tests/docker_bench.sh tests/docker_upgrade.sh tests/cnpg_smoke.sh tests/cnpg_e2e.sh \
  tests/soak.sh tests/soak/proc_sample.sh \
  docker/initdb-pg-automerge.sh || fail "shellcheck"

# job NAME [FILE]: the lines of that job (from "  NAME:" to the next job).
job() { awk -v j="  $1:" '$0 == j {p=1; print; next} p && /^  [a-z]/ {exit} p' "${2:-$wf}"; }
for j in ci docs docker cnpg-e2e; do [ -n "$(job "$j")" ] || fail "$wf: no $j job"; done
docker_job="$(job docker)"; e2e_job="$(job cnpg-e2e)"; ci_job="$(job ci)"
has() { grep -qE -- "$2" <<<"$1"; }

# Runners pinned to ubuntu-24.04 / ubuntu-24.04-arm everywhere.
if grep -hE '^\s+runs-on:' "$wf" "$rel" "$docs_wf" | grep -E 'latest' >/dev/null; then fail "pin runners to ubuntu-24.04 / ubuntu-24.04-arm, not *-latest"; fi

# ci.yml: push to main, pull requests, nightly and manual runs, and calls
# from release.yml (workflow_call with the inputs release and ref); no tag
# trigger and nothing that publishes.
on_block="$(awk '/^on:$/ {p=1; next} p && /^[a-z]/ {exit} p' "$wf")"
has "$on_block" '^  push:$' && has "$on_block" '^    branches: \[main\]$' && has "$on_block" '^  pull_request:$' \
  && has "$on_block" '^  schedule:$' && has "$on_block" '^  workflow_dispatch:$' && has "$on_block" '^  workflow_call:$' \
  || fail "$wf: on: push (main), pull_request, schedule, workflow_dispatch and workflow_call"
has "$on_block" '^      release:$' && has "$on_block" '^      ref:$' || fail "$wf: workflow_call inputs release and ref"
if has "$on_block" 'tags'; then fail "$wf: no tag trigger (releases are published by $rel)"; fi
if [ -n "$(job publish)" ]; then fail "$wf: no publish job (publishing is $rel's)"; fi
if grep -vE '^ *#' "$wf" | grep -qE 'docker/login-action|docker-push\.sh|release-publish\.sh|docker push|imagetools|gh release|github\.token|secrets\.'; then
  fail "$wf: logs in, pushes, edits releases or uses a token (only $rel's publish job may)"
fi
if grep -qE '^ +[a-z-]+: write$|write-all|push: true' "$wf"; then fail "$wf: write permissions or push: true"; fi
grep -qE '^permissions:$' "$wf" && grep -qE '^  contents: read$' "$wf" || fail "$wf: top-level permissions must be contents: read"
# Every job checks out the release's commit when called with one.
[ "$(grep -cE '^      - uses: actions/checkout@v7$' "$wf")" = "$(grep -cE '^          ref: \$\{\{ inputs\.ref \}\}$' "$wf")" ] \
  || fail "$wf: every checkout must use ref: \${{ inputs.ref }}"
# The release-only steps are gated on inputs.release, not on refs.
if grep -qF "refs/tags/" "$wf"; then fail "$wf: release steps are gated on inputs.release, not on tag refs"; fi
has "$ci_job" '^        run: PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config mise run package$' \
  || fail "$wf: the ci job must package against PGDG's Postgres 18 for a release"
has "$ci_job" '^          name: pg_automerge-pg18$' || fail "$wf: the ci job must upload the package as pg_automerge-pg18"

# docker: runs on every push and PR (no job-level if), amd64 always and
# arm64 for a release on the native runner, the layer cache, the tests
# with the PGDG client tools, and the archives for a release.
if has "$docker_job" '^    if:'; then fail "$wf: the docker job must run for every event"; fi
has "$docker_job" "^        arch: \\$\\{\\{ fromJSON\\(inputs\\.release && '\\[\"amd64\", \"arm64\"\\]' \\|\\| '\\[\"amd64\"\\]'\\) \\}\\}$" \
  || fail "$wf: docker matrix must be amd64, plus arm64 for a release"
has "$docker_job" "^    runs-on: \\$\\{\\{ matrix\\.arch == 'arm64' && 'ubuntu-24\\.04-arm' \\|\\| 'ubuntu-24\\.04' \\}\\}$" \
  || fail "$wf: arm64 must build on the native arm64 runner (ubuntu-24.04-arm)"
has "$docker_job" 'bash scripts/docker-build.sh --full -- --load ' || fail "$wf: build the full image with scripts/docker-build.sh --full -- --load"
has "$docker_job" 'bash scripts/docker-build.sh --cnpg -- --load ' || fail "$wf: build the CNPG image with scripts/docker-build.sh --cnpg -- --load"
has "$docker_job" 'PG_AUTOMERGE_CNPG_IMAGE=\$cnpg" >>"\$GITHUB_ENV"' || fail "$wf: hand the CNPG image's tag to the later steps"
has "$docker_job" 'bash scripts/docker-archive.sh "\$PG_AUTOMERGE_CNPG_IMAGE" dist' || fail "$wf: a release must save the CNPG image too"
has "$docker_job" 'bash scripts/docker-archive.sh "" dist' || fail "$wf: a release must save the full image (scripts/docker-archive.sh)"
has "$docker_job" '--cache-from "type=gha,scope=pg-automerge-\$\{\{ matrix.arch \}\}"' || fail "$wf: cache-from type=gha per arch"
has "$docker_job" '--cache-to "type=gha,scope=pg-automerge-\$\{\{ matrix.arch \}\},mode=max' || fail "$wf: cache-to type=gha,mode=max per arch"
has "$docker_job" '^        run: bash tests/docker.sh$' || fail "$wf: the docker job must run tests/docker.sh"
has "$docker_job" '^          PG_CONFIG: /usr/lib/postgresql/18/bin/pg_config$' || fail "$wf: the docker tests use PGDG's client tools"
has "$docker_job" 'DOCKER_TEST_SUITES' && fail "$wf: the docker job must run the suites"
[ "$(grep -cxF '        if: inputs.release' <<<"$docker_job")" = 3 ] || fail "$wf: the docker job saves and uploads both archives for a release only"

# cnpg-e2e: the docker job's own CNPG image (saved by it under the same
# condition), tests/cnpg_e2e.sh with the pinned kind and kubectl; for a
# release, nightly, manual runs, or always if the repository says so; its
# last step sets the output release.yml's publish requires.
has "$e2e_job" '^    needs: docker$' || fail "$wf: cnpg-e2e must need docker"
e2e_if="inputs.release || github.event_name == 'schedule' || github.event_name == 'workflow_dispatch' || vars.PG_AUTOMERGE_CNPG_E2E == 'always'"
[ "$(grep -cxF "    if: $e2e_if" <<<"$e2e_job")" = 1 ] \
  || fail "$wf: cnpg-e2e must run for a release, on schedule, workflow_dispatch or with PG_AUTOMERGE_CNPG_E2E=always"
[ "$(grep -cxF "        if: matrix.arch == 'amd64' && ($e2e_if)" <<<"$docker_job")" = 2 ] \
  || fail "$wf: the docker job must save and upload the CNPG image for cnpg-e2e under cnpg-e2e's condition"
has "$docker_job" 'docker image save "\$PG_AUTOMERGE_CNPG_IMAGE" \| gzip >cnpg-e2e-image\.tar\.gz$' \
  || fail "$wf: the docker job must save its CNPG image for cnpg-e2e"
has "$e2e_job" '^          name: cnpg-e2e-image$' || fail "$wf: cnpg-e2e must download the docker job's CNPG image"
has "$e2e_job" '^          install_args: aqua:kubernetes-sigs/kind kubectl$' || fail "$wf: cnpg-e2e installs kind and kubectl from mise.toml"
has "$e2e_job" '^        run: bash tests/cnpg_e2e.sh$' || fail "$wf: cnpg-e2e must run tests/cnpg_e2e.sh"
[ "$(grep -E '^      - ' <<<"$e2e_job" | tail -1)" = '      - name: Passed' ] && has "$e2e_job" '^      passed: \$\{\{ steps\.passed\.outputs\.passed \}\}$' \
  || fail "$wf: cnpg-e2e's last step must set its passed output"
grep -qxF '        value: ${{ jobs.cnpg-e2e.outputs.passed }}' "$wf" || fail "$wf: workflow_call output cnpg-e2e-passed from cnpg-e2e"

# release.yml: on a published release (and manual re-runs with a tag);
# check, then ci.yml with release: true on the tag's commit, then publish,
# which needs both, requires cnpg-e2e's output, runs in the release
# environment and is the only job with write permissions, a registry
# login, a push or gh release.
for j in check test publish; do [ -n "$(job "$j" "$rel")" ] || fail "$rel: no $j job"; done
check_job="$(job check "$rel")"; test_job="$(job test "$rel")"; rpublish_job="$(job publish "$rel")"
rel_on="$(awk '/^on:$/ {p=1; next} p && /^[a-z]/ {exit} p' "$rel")"
has "$rel_on" '^  release:$' && has "$rel_on" '^    types: \[published\]$' || fail "$rel: on: release: types: [published]"
has "$rel_on" '^  workflow_dispatch:$' && has "$rel_on" '^      tag:$' || fail "$rel: workflow_dispatch with a tag input"
[ "$(grep -cE '^  [a-z_]+:' <<<"$rel_on")" = 2 ] || fail "$rel: only the release and workflow_dispatch triggers"
grep -qE '^  cancel-in-progress: false$' "$rel" || fail "$rel: never cancel a release run part way"
has "$check_job" 'if \[ "\$TAG" != "v\$version" \]; then$' || fail "$rel: check must compare the tag with Cargo.toml's version"
has "$check_job" '^          ref: refs/tags/\$\{\{ steps\.release\.outputs\.tag \}\}$' || fail "$rel: check reads the version at the tag"
has "$check_job" '^      prerelease: ' && has "$check_job" '^      version: ' && has "$check_job" '^      sha: ' \
  || fail "$rel: check outputs version, sha and prerelease"
has "$test_job" '^    needs: check$' && has "$test_job" '^    uses: \./\.github/workflows/ci\.yml$' \
  && has "$test_job" '^      release: true$' && has "$test_job" '^      ref: \$\{\{ needs\.check\.outputs\.sha \}\}$' \
  || fail "$rel: test must call ci.yml with release: true on the checked commit"
has "$rpublish_job" '^    needs: \[check, test\]$' || fail "$rel: publish must need check and test (all of ci.yml's jobs)"
has "$rpublish_job" "^    if: needs\\.test\\.outputs\\.cnpg-e2e-passed == 'true'$" || fail "$rel: publish only after cnpg-e2e passed"
has "$rpublish_job" '^      name: release$' && has "$rpublish_job" '^    environment:$' || fail "$rel: publish runs in the release environment"
[ "$(grep -cE '^ +[a-z-]+: write$' <<<"$rpublish_job")" = 2 ] && has "$rpublish_job" '^      packages: write$' \
  && has "$rpublish_job" '^      contents: write$' || fail "$rel: publish: permissions exactly contents: write and packages: write"
has "$rpublish_job" '^          registry: ghcr\.io$' && has "$rpublish_job" '^          password: \$\{\{ github\.token \}\}$' \
  || fail "$rel: publish logs in to ghcr.io with GITHUB_TOKEN"
has "$rpublish_job" 'bash scripts/release-publish\.sh "\$\{latest\[@\]\}" "\$TAG" "\$SHA" "\$GITHUB_REPOSITORY" dist$' \
  || fail "$rel: publish runs scripts/release-publish.sh"
has "$rpublish_job" 'if \[ "\$PRERELEASE" != true \] && \[ "\$newest" = "\$TAG" \]; then$' \
  || fail "$rel: :latest only for the latest release, never a prerelease"
rel_outside="$(awk '$0 == "  publish:" {p=1; next} p && /^  [a-z]/ {p=0} !p' "$rel" | grep -vE '^ *#')"
if has "$rel_outside" 'docker/login-action|docker-push\.sh|release-publish\.sh|docker push|imagetools|gh release (upload|edit|create)|: write$|write-all'; then
  fail "$rel: login, pushes, release edits or write permissions outside publish"
fi
grep -qE '^permissions:$' "$rel" && grep -qE '^  contents: read$' "$rel" || fail "$rel: top-level permissions must be contents: read"
if grep -qE 'secrets\.' "$rel"; then fail "$rel: no secrets (GITHUB_TOKEN only)"; fi
# The old opt-in machinery is gone everywhere.
stale="$(git grep -lE 'PG_AUTOMERGE_(PUBLISH|REGISTRY_|ARM64|PUSH_LATEST)' -- ':!tests/check_ci.sh' || true)"
[ -z "$stale" ] || fail "the publish opt-in variables are gone (release.yml publishes to ghcr.io), still named in: $stale"
# Actions at their current majors.
for a in actions/checkout@v7 actions/cache@v6 actions/upload-artifact@v7 actions/download-artifact@v8 \
         docker/setup-buildx-action@v4 docker/login-action@v4 crazy-max/ghaction-github-runtime@v4 jdx/mise-action@v5; do
  name="${a%@*}"
  if grep -hoE "uses: $name@[^ ]+" "$wf" "$rel" "$docs_wf" | grep -vqxF "uses: $a"; then fail "use $a"; fi
done

# release-publish.sh: checks both image sets (dry runs, amd64 and arm64,
# revision and source) before pushing, CNPG before the full image (whose
# index may move :latest), then the assets and the notes.
rp=scripts/release-publish.sh
order="$(grep -oE '^ *bash scripts/docker-push\.sh( --dry-run)?( --cnpg)? |^run gh release (upload|edit)' "$rp" | sed -E 's/^ +//; s/ $//' | tr '\n' '|')"
[ "$order" = 'bash scripts/docker-push.sh --dry-run --cnpg|bash scripts/docker-push.sh --dry-run|bash scripts/docker-push.sh --cnpg|bash scripts/docker-push.sh|run gh release upload|run gh release edit|' ] \
  || fail "$rp: dry-run both image sets, push CNPG, then the full image, then assets and notes (got: $order)"
grep -qxF 'check=(--arch amd64 --arch arm64 --revision "$revision" --source "$source_url")' "$rp" \
  || fail "$rp: require amd64 and arm64 and the tag's revision and source"
grep -qF -- '--clobber' "$rp" || fail "$rp: gh release upload --clobber (re-runs)"
# release-notes.sh replaces its section: idempotent, the author's notes kept.
notes_args=(0.1.0 ghcr.io/o/pg-automerge ghcr.io/o/pg-automerge-cnpg trixie 0123456789abcdef0123456789abcdef01234567)
n1="$(printf 'My notes\r\n\r\n- a\n\n' | bash scripts/release-notes.sh "${notes_args[@]}")"
n2="$(bash scripts/release-notes.sh "${notes_args[@]}" <<<"$n1")"
[ "$n1" = "$n2" ] || fail "scripts/release-notes.sh is not idempotent"
[ "$(grep -c 'pg-automerge-release:begin' <<<"$n2")" = 1 ] && [ "$(head -1 <<<"$n2")" = 'My notes' ] \
  && grep -qxF 'docker pull ghcr.io/o/pg-automerge:0.1.0' <<<"$n2" \
  && grep -qxF '          reference: ghcr.io/o/pg-automerge-cnpg:0.1.0-18-trixie' <<<"$n2" \
  || fail "scripts/release-notes.sh: one section after the notes, with the pull commands and the CNPG reference"
# The release tag is refused unless it is v<Cargo.toml version>.
crate_version="$(sed -n 's/^CRATE_VERSION=//p' <<<"$versions")"
mkdir -p "$tmp/dist"
if out="$(bash "$rp" --dry-run v0.0.0-nope 0123456789abcdef0123456789abcdef01234567 o/r "$tmp/dist" 2>&1)"; then fail "$rp accepted a wrong tag"; fi
grep -qF "must be v$crate_version" <<<"$out" || fail "$rp: expected a tag/version error, got: $out"

echo "check_ci: workflow ok"

# ---------------------------------------------------------------------------
# The documentation site (docs/, a pnpm package; docs/Readme.md): checked
# by the docs job and `mise run ci`, published from main by
# deploy-docs.yml, and never part of the Docker build context.
docs_job="$(job docs)"
has "$docs_job" '^        run: mise run docs-check$' || fail "$wf: the docs job must run mise run docs-check"
has "$docs_job" '^          install_args: node pnpm$' || fail "$wf: the docs job installs only node and pnpm"
has "$docs_job" 'MISE_TASK_RUN_AUTO_INSTALL: "false"' \
  || fail "$wf: the docs job must not let mise run install the Rust toolchain"
grep -qE '^  "mise run docs-check",$' mise.toml || fail "mise.toml: mise run ci must run docs-check"
for t in docs-install docs-dev docs-build docs-check; do
  grep -qxF "[tasks.$t]" mise.toml || fail "mise.toml: no $t task"
done
for f in package.json pnpm-lock.yaml ssg.tsx ssg-for-vite.tsx src/ssg-main.tsx src/main.tsx src/routes.tsx scripts/check-site.ts; do
  [ -f "docs/$f" ] || fail "docs/$f missing"
done
[ -x docs/ssg.tsx ] || fail "docs/ssg.tsx must be executable (pnpm build runs it)"
grep -qE '^      - "docs/\*\*"$' "$docs_wf" || fail "$docs_wf: must run on changes to docs/**"
grep -qE '^      - CHANGELOG\.md$' "$docs_wf" || fail "$docs_wf: must run on changes to CHANGELOG.md (the changelog page)"
grep -qE '^        run: pnpm --dir docs install --frozen-lockfile$' "$docs_wf" || fail "$docs_wf: install with --frozen-lockfile"
[ "$(grep -cE '^          DOCS_BASE: /\$\{\{ github\.event\.repository\.name \}\}/$' "$docs_wf")" -eq 2 ] \
  || fail "$docs_wf: build and check-site with DOCS_BASE=/<repo>/"
grep -qE '^          path: docs/dist$' "$docs_wf" || fail "$docs_wf: upload docs/dist"
if grep -qE '^!/?docs' .dockerignore; then fail ".dockerignore: keep docs/ out of the Docker build context"; fi
grep -qF '<Include file="../../../CHANGELOG.md" />' docs/src/pages/changelog.mdx \
  || fail "docs/src/pages/changelog.mdx: must include CHANGELOG.md (not a copy of it)"
# The documents live in docs/src/pages now: every page path named anywhere
# in the repository (code comments, tests, CHANGELOG.md, README.md) exists,
# and nothing points at the removed design document (only docs/Readme.md,
# which says where the content came from, names it).
while read -r ref; do
  [ -f "$ref" ] || fail "a reference to $ref, which does not exist: $(git grep -lF "$ref" | tr '\n' ' ')"
done < <(git grep -ohE 'docs/src/pages/[A-Za-z0-9_./-]+\.mdx' | sort -u)
stale="$(git grep -lE '(docs/)?DESIGN\.md' -- ':!docs/Readme.md' || true)"
[ -z "$stale" ] || fail "references to the removed design document (now docs/src/pages): $stale"
# tests/docker_upgrade.sh runs the compose commands of the Updating page.
grep -qxF 'UPDATING_PAGE=docs/src/pages/guide/updating.mdx' tests/docker_upgrade.sh \
  || fail "tests/docker_upgrade.sh must read the Updating page's commands"
grep -qxF '**With compose, step by step**' docs/src/pages/guide/updating.mdx \
  || fail "docs/src/pages/guide/updating.mdx: no **With compose, step by step** section"

echo "check_ci: docs ok"
