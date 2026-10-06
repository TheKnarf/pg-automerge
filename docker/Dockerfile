# syntax=docker/dockerfile:1
#
# The official postgres:18 image plus pg_automerge, built against the PGDG
# Postgres 18 of that very image (not pgrx's assert-enabled development
# build). Build from the repository root (the build context):
#
#   mise run docker-build
#   # or by hand:
#   docker build -f docker/Dockerfile \
#     --build-arg PG_AUTOMERGE_VERSION="$(sh scripts/versions.sh | sed -n 's/^CRATE_VERSION=//p')" \
#     -t pg-automerge:dev .
#
# Toolchain versions are not repeated here: the `versions` stage reads Rust
# and cargo-pgrx from mise.toml (and checks cargo-pgrx against the pgrx in
# Cargo.lock) with scripts/versions.sh. See docs/src/pages/guide/docker.mdx
# and docs/src/pages/design/docker-image.mdx.
#
# Two images come out of one builder: the default target, runtime (the
# last stage), and cnpg-extension, the CloudNativePG image volume extension
# (mise run docker-build builds both; scripts/docker-build.sh --cnpg only
# that one; docs/src/pages/guide/cloudnativepg.mdx).

# The base of both the builder and the runtime stage, so the Debian release,
# glibc and the Postgres headers the extension is compiled against are the
# runtime's. Pinned by the digest of the multi-arch index; the tag is kept
# for readers (Docker ignores it when a digest is given). To take a new
# Postgres minor release or Debian security updates, replace the digest with
# the one printed by:
#   docker buildx imagetools inspect postgres:18-trixie
# (or build with --build-arg PG_IMAGE=postgres:18-trixie to follow the tag).
ARG PG_IMAGE=postgres:18-trixie@sha256:5a5a84b19854a9ffaa54082c166ff4ec27473a361e496e5ea167f298f2da9722

# ---------------------------------------------------------------------------
# versions: the pinned toolchain, read from the repository's own files by
# scripts/versions.sh.
# The builder copies only /toolchain.env from here, and BuildKit keys that
# COPY by the file's content, so editing mise.toml (a task, say) does not
# rebuild the toolchain unless a version changed.
FROM ${PG_IMAGE} AS versions
ARG PG_AUTOMERGE_VERSION
COPY mise.toml Cargo.toml Cargo.lock /v/
COPY scripts/versions.sh /v/scripts/
RUN <<'EOF'
set -eu
sh /v/scripts/versions.sh /v >/v/all.env
. /v/all.env
fail() { echo "versions: $*" >&2; exit 1; }
[ -n "${PG_AUTOMERGE_VERSION:-}" ] || fail "pass --build-arg PG_AUTOMERGE_VERSION=$CRATE_VERSION (mise run docker-build does)"
[ "$PG_AUTOMERGE_VERSION" = "$CRATE_VERSION" ] || fail "PG_AUTOMERGE_VERSION=$PG_AUTOMERGE_VERSION != $CRATE_VERSION (Cargo.toml)"
grep -E '^(RUST|CARGO_PGRX)_VERSION=' /v/all.env >/toolchain.env
echo "$CRATE_VERSION" >/version
cat /v/all.env
EOF

# ---------------------------------------------------------------------------
# builder: Postgres 18 server headers from the PGDG repository the base
# image already configures, Rust and cargo-pgrx as pinned, then a release
# package made by scripts/package.sh (the same script `mise run package`
# and tag CI use).
FROM ${PG_IMAGE} AS builder
ENV RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PGRX_HOME=/opt/pgrx \
    PATH=/opt/cargo/bin:$PATH \
    CARGO_TERM_COLOR=never
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# Build dependencies. The headers are pinned to the base image's own
# Postgres ($PG_VERSION, set by the official image, e.g. 18.6-1.pgdg13+2).
# PGDG normally keeps it; if a future base's version has been dropped from
# the repository, the newest 18.x headers are used instead (minor releases
# keep the server ABI), and the build log says so.
RUN --mount=type=cache,id=pg-automerge-apt-cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=pg-automerge-apt-lists,target=/var/lib/apt/lists,sharing=locked <<'EOF'
set -eux
rm -f /etc/apt/apt.conf.d/docker-clean
apt-get update
if ! apt-get install -y --no-install-recommends "postgresql-server-dev-$PG_MAJOR=$PG_VERSION"; then
  echo "WARNING: postgresql-server-dev-$PG_MAJOR=$PG_VERSION is not available; using the newest $PG_MAJOR.x headers" >&2
  apt-get install -y --no-install-recommends "postgresql-server-dev-$PG_MAJOR"
fi
# libclang for pgrx's bindgen; libicu-dev because the server headers include
# ICU's (Debian's, as the runtime links); rustup from Debian (signed apt),
# which then installs the pinned toolchain.
apt-get install -y --no-install-recommends \
  build-essential clang libclang-dev libicu-dev pkg-config ca-certificates rustup
EOF

# Rust and cargo-pgrx, keyed only by the versions (see the versions stage).
COPY --from=versions /toolchain.env /toolchain.env
RUN --mount=type=cache,id=pg-automerge-cargo-registry,target=/opt/cargo/registry \
    --mount=type=cache,id=pg-automerge-cargo-git,target=/opt/cargo/git <<'EOF'
set -eux
. /toolchain.env
rustup toolchain install "$RUST_VERSION" --profile minimal
rustup default "$RUST_VERSION"
cargo install --locked cargo-pgrx --version "$CARGO_PGRX_VERSION"
cargo pgrx --version
# pgrx builds against the image's PGDG Postgres, never one of its own.
cargo pgrx init --pg$PG_MAJOR "/usr/lib/postgresql/$PG_MAJOR/bin/pg_config"
EOF

WORKDIR /src
COPY . .
# target/ is a cache mount (warm rebuilds reuse compiled dependencies), so
# the package is copied out to /out within the same step, laid out by
# destination: /out/lib (pkglibdir) and /out/extension (sharedir/extension).
# Only the library, the control file and SQL scripts may be in it.
RUN --mount=type=cache,id=pg-automerge-cargo-registry,target=/opt/cargo/registry \
    --mount=type=cache,id=pg-automerge-cargo-git,target=/opt/cargo/git \
    --mount=type=cache,id=pg-automerge-target,target=/src/target <<'EOF'
set -eux
pg_config=/usr/lib/postgresql/$PG_MAJOR/bin/pg_config
PG_CONFIG=$pg_config bash scripts/package.sh
pkg=target/release/pg_automerge-pg$PG_MAJOR
pkglibdir=$($pg_config --pkglibdir)
extdir=$($pg_config --sharedir)/extension
# The runtime stage copies to these fixed paths.
[ "$pkglibdir" = "/usr/lib/postgresql/$PG_MAJOR/lib" ]
[ "$extdir" = "/usr/share/postgresql/$PG_MAJOR/extension" ]
mkdir -p /out/lib /out/extension
cp "$pkg$pkglibdir/pg_automerge.so" /out/lib/
cp "$pkg$extdir"/pg_automerge.control "$pkg$extdir"/pg_automerge--*.sql /out/extension/
extra=$(cd "$pkg" && find . -type f ! -name pg_automerge.so ! -name pg_automerge.control ! -name 'pg_automerge--*.sql')
[ -z "$extra" ] || { echo "unexpected files in the package: $extra" >&2; exit 1; }
ls -l /out/lib /out/extension
EOF

# ---------------------------------------------------------------------------
# cnpg-files: the builder's files laid out as a CloudNativePG image volume
# extension wants them (/lib, /share/extension; the operator mounts the
# image at /extensions/<name> and sets extension_control_path and
# dynamic_library_path to its share and lib), checked against the base's
# Debian release, which names the image (PG_DEBIAN: scripts/docker-build.sh
# passes it, read from PG_IMAGE's tag). Only the cnpg-extension target
# builds this; the default target (runtime) never does.
FROM ${PG_IMAGE} AS cnpg-files
ARG PG_AUTOMERGE_VERSION
ARG PG_DEBIAN
COPY --from=builder /out/lib/pg_automerge.so /cnpg/lib/
COPY --from=builder /out/extension/ /cnpg/share/extension/
# The MIT notice travels with every copy (CNPG's extension images keep
# theirs in /licenses/<package>/).
COPY LICENSE /cnpg/licenses/pg_automerge/copyright
RUN <<'EOF'
set -eu
fail() { echo "cnpg-files: $*" >&2; exit 1; }
# Root-owned and read-only for everyone else: Postgres (uid 26 in CNPG's
# images) only reads them. Directories need their x bit, which COPY
# --chmod would take from the parents it creates.
chown -R 0:0 /cnpg
find /cnpg -type d -exec chmod 0755 {} +
find /cnpg -type f -exec chmod 0644 {} +
chmod 0755 /cnpg/lib/pg_automerge.so
. /etc/os-release
[ -n "${PG_DEBIAN:-}" ] || fail "pass --build-arg PG_DEBIAN=$VERSION_CODENAME (mise run docker-build-cnpg does)"
[ "$PG_DEBIAN" = "$VERSION_CODENAME" ] || fail "PG_DEBIAN=$PG_DEBIAN, but the base image is Debian $VERSION_CODENAME"
[ "$PG_MAJOR" = 18 ] || fail "the base image is Postgres $PG_MAJOR, not 18"
# The io.cloudnativepg.image.sql.version label is the crate version: it
# must be the control file's default_version.
grep -qxF "default_version = '$PG_AUTOMERGE_VERSION'" /cnpg/share/extension/pg_automerge.control \
  || fail "default_version in pg_automerge.control is not $PG_AUTOMERGE_VERSION"
# A bare module_pathname is what lets dynamic_library_path find the library
# in the image volume ('$libdir/pg_automerge' would not).
grep -qxF "module_pathname = 'pg_automerge'" /cnpg/share/extension/pg_automerge.control \
  || fail "module_pathname must be the bare 'pg_automerge'"
if grep -lF '$libdir' /cnpg/share/extension/*.sql; then fail "SQL scripts must not name \$libdir"; fi
find /cnpg -printf '%M %u %p\n' | sort -k3
EOF

# ---------------------------------------------------------------------------
# cnpg-extension: the CloudNativePG extension image, `--target
# cnpg-extension`. Files only, from scratch (it cannot be run; CNPG mounts
# it read-only into its own PostgreSQL 18 operand image of the same Debian
# release and architecture). See docs/src/pages/guide/cloudnativepg.mdx.
FROM scratch AS cnpg-extension
ARG PG_AUTOMERGE_VERSION
ARG PG_DEBIAN
ARG PG_AUTOMERGE_SOURCE=
ARG PG_AUTOMERGE_REVISION=
LABEL org.opencontainers.image.title="pg_automerge (CloudNativePG extension)" \
      org.opencontainers.image.description="The pg_automerge extension as a CloudNativePG image volume extension for PostgreSQL 18 on Debian ${PG_DEBIAN} (mount it with Cluster spec.postgresql.extensions)" \
      org.opencontainers.image.version="${PG_AUTOMERGE_VERSION}" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.source="${PG_AUTOMERGE_SOURCE}" \
      org.opencontainers.image.revision="${PG_AUTOMERGE_REVISION}" \
      org.opencontainers.image.base.name="scratch" \
      io.cloudnativepg.image.base.name="ghcr.io/cloudnative-pg/postgresql:18-minimal-${PG_DEBIAN}" \
      io.cloudnativepg.image.base.pgmajor="18" \
      io.cloudnativepg.image.base.os="${PG_DEBIAN}" \
      io.cloudnativepg.image.sql.version="${PG_AUTOMERGE_VERSION}"
COPY --from=cnpg-files /cnpg/ /
# Nobody runs it; a non-root user keeps image scanners quiet (CNPG's own
# extension images do the same).
USER 65532:65532

# ---------------------------------------------------------------------------
# runtime: the untouched official image (entrypoint, users, volume, signals)
# plus the library, the control file and SQL scripts, the init script, and
# the license. No toolchain, no build dependencies.
FROM ${PG_IMAGE} AS runtime
ARG PG_IMAGE
ARG PG_AUTOMERGE_VERSION
# The repository URL and commit, for the labels (mise run docker-build
# passes `git remote get-url origin`, empty while there is no remote, and
# `git rev-parse HEAD`).
ARG PG_AUTOMERGE_SOURCE=
ARG PG_AUTOMERGE_REVISION=
LABEL org.opencontainers.image.title="pg_automerge" \
      org.opencontainers.image.description="PostgreSQL 18 (official postgres image) with the pg_automerge extension: Automerge CRDT documents with jsonb queries, merges, history and change notifications" \
      org.opencontainers.image.version="${PG_AUTOMERGE_VERSION}" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.source="${PG_AUTOMERGE_SOURCE}" \
      org.opencontainers.image.revision="${PG_AUTOMERGE_REVISION}" \
      org.opencontainers.image.base.name="${PG_IMAGE}"
COPY --from=builder --chmod=0755 /out/lib/pg_automerge.so /usr/lib/postgresql/18/lib/
COPY --from=builder --chmod=0644 /out/extension/ /usr/share/postgresql/18/extension/
# The MIT license asks for its notice in every copy (Debian's place for it).
COPY --from=versions --chmod=0644 /version /usr/share/doc/pg_automerge/VERSION
COPY --chmod=0644 LICENSE /usr/share/doc/pg_automerge/copyright
# The init script, and a copy outside /docker-entrypoint-initdb.d that
# survives a bind mount over that directory (docs/src/pages/guide/docker.mdx).
COPY --chmod=0755 docker/initdb-pg-automerge.sh /docker-entrypoint-initdb.d/10-pg-automerge.sh
COPY --chmod=0755 docker/initdb-pg-automerge.sh /usr/local/bin/pg-automerge-initdb
