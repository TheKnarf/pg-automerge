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
# The workflow: actionlint (syntax, expressions, runner labels, and
# running shellcheck on every run: script), shellcheck on the scripts CI
# runs, and the invariants of the docker and publish jobs.
command -v actionlint >/dev/null && command -v shellcheck >/dev/null \
  || fail "actionlint and shellcheck are needed (pinned in mise.toml: mise install)"
docs_wf=.github/workflows/deploy-docs.yml
actionlint "$wf" "$docs_wf" || fail "$wf, $docs_wf: actionlint"
shellcheck -x -S warning scripts/*.sh tests/check_ci.sh tests/docker.sh tests/docker_lib.sh tests/docker_bench.sh tests/docker_upgrade.sh tests/cnpg_smoke.sh tests/cnpg_e2e.sh \
  tests/soak.sh tests/soak/proc_sample.sh \
  docker/initdb-pg-automerge.sh || fail "shellcheck"

# job NAME: the lines of that job (from "  NAME:" to the next job).
job() { awk -v j="  $1:" '$0 == j {p=1; print; next} p && /^  [a-z]/ {exit} p' "$wf"; }
for j in ci docs docker cnpg-e2e publish; do [ -n "$(job "$j")" ] || fail "$wf: no $j job"; done
docker_job="$(job docker)"; publish_job="$(job publish)"; e2e_job="$(job cnpg-e2e)"
has() { grep -qE -- "$2" <<<"$1"; }

# docker: runs on every push and PR (no job-level if), amd64 always and
# arm64 on tags on the native runner, the layer cache, the tests with the
# PGDG client tools, the tag/version check and the artifact.
if has "$docker_job" '^    if:'; then fail "$wf: the docker job must run for every event"; fi
has "$docker_job" "^        arch: .*startsWith\(github\.ref, 'refs/tags/'\).*'\[\"amd64\", \"arm64\"\]'.*'\[\"amd64\"\]'" \
  || fail "$wf: docker matrix must be amd64, plus arm64 on tags"
has "$docker_job" "^    runs-on: .*'ubuntu-24.04-arm'" || fail "$wf: arm64 must build on the native arm64 runner"
has "$docker_job" 'bash scripts/docker-build.sh --full -- --load ' || fail "$wf: build the full image with scripts/docker-build.sh --full -- --load"
has "$docker_job" 'bash scripts/docker-build.sh --cnpg -- --load ' || fail "$wf: build the CNPG image with scripts/docker-build.sh --cnpg -- --load"
has "$docker_job" 'PG_AUTOMERGE_CNPG_IMAGE=\$cnpg" >>"\$GITHUB_ENV"' || fail "$wf: hand the CNPG image's tag to the later steps"
has "$docker_job" 'bash scripts/docker-archive.sh "\$PG_AUTOMERGE_CNPG_IMAGE" dist' || fail "$wf: tags must save the CNPG image too"
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
has "$publish_job" '^    needs: \[ci, docker, cnpg-e2e\]$' || fail "$wf: publish must need ci, docker and cnpg-e2e"
# cnpg-e2e: the docker job's own CNPG image (saved by it, every event),
# tests/cnpg_e2e.sh with the pinned kind and kubectl.
has "$e2e_job" '^    needs: docker$' || fail "$wf: cnpg-e2e must need docker"
if has "$e2e_job" '^    if:'; then fail "$wf: the cnpg-e2e job must run for every event"; fi
has "$docker_job" 'docker image save "\$PG_AUTOMERGE_CNPG_IMAGE" \| gzip >cnpg-e2e-image\.tar\.gz$' \
  || fail "$wf: the docker job must save its CNPG image for cnpg-e2e"
has "$e2e_job" '^          name: cnpg-e2e-image$' || fail "$wf: cnpg-e2e must download the docker job's CNPG image"
has "$e2e_job" '^          install_args: aqua:kubernetes-sigs/kind kubectl$' || fail "$wf: cnpg-e2e installs kind and kubectl from mise.toml"
has "$e2e_job" '^        run: bash tests/cnpg_e2e.sh$' || fail "$wf: cnpg-e2e must run tests/cnpg_e2e.sh"
has "$publish_job" '^          if \[ -z "\$REGISTRY_TOKEN" \]; then$' || fail "$wf: publish must check the secret"
steps=$(grep -cE '^      - ' <<<"$publish_job")
gated=$(grep -cE "^        if: steps\.cfg\.outputs\.push == 'true'$" <<<"$publish_job")
[ "$gated" -eq $((steps - 1)) ] || fail "$wf: publish: $gated of $((steps - 1)) steps after the check are gated on it"
outside="$(awk -v j="  publish:" '$0 == j {p=1} p && /^  [a-z]/ && $0 != j {p=0} !p' "$wf")"
if has "$outside" 'docker/login-action|docker-push\.sh|docker push|imagetools'; then fail "$wf: pushing outside the publish job"; fi
has "$publish_job" 'bash scripts/docker-push.sh "\$REGISTRY_IMAGE" dist/pg-automerge-\[0-9\]\*-linux-\*\.tar\.gz$' \
  || fail "$wf: publish must push the full image archives (pg-automerge-[0-9]*) without --cnpg"
has "$publish_job" 'bash scripts/docker-push.sh --cnpg .* dist/pg-automerge-cnpg-\*-linux-\*\.tar\.gz$' \
  || fail "$wf: publish must push the CNPG archives with --cnpg"
if grep -qE 'push: true|packages: write|write-all' "$wf"; then fail "$wf: no push: true or write permissions"; fi
grep -qE '^permissions:$' "$wf" && grep -qE '^  contents: read$' "$wf" || fail "$wf: top-level permissions must be contents: read"

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
grep -qE '^\*\*From .* to [0-9.]+ with compose\*\*' docs/src/pages/guide/updating.mdx \
  || fail "docs/src/pages/guide/updating.mdx: no **From ... to <version> with compose** steps"

echo "check_ci: docs ok"
