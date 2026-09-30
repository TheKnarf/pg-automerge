#!/usr/bin/env bash
# Build the Docker image (docker/Dockerfile) as pg-automerge:<version> and
# pg-automerge:dev, passing what the Dockerfile cannot read by itself: the
# crate version (checked against Cargo.toml inside the build) and, for the
# OCI labels, the repository URL and commit. Never pushes.
#
# Env: PG_AUTOMERGE_IMAGE (default pg-automerge), extra `docker build`
# arguments after `--`, e.g. `mise run docker-build -- --no-cache`.
set -euo pipefail
cd "$(dirname "$0")/.."

image="${PG_AUTOMERGE_IMAGE:-pg-automerge}"
version="$(sh scripts/versions.sh | sed -n 's/^CRATE_VERSION=//p')"
# Normalised: no credentials of an https remote in the public label.
source_url="$(bash scripts/oci-source-url.sh)"
revision="$(git rev-parse HEAD 2>/dev/null || true)"
if [[ -n "$revision" ]] && ! git diff --quiet HEAD 2>/dev/null; then revision="$revision-dirty"; fi

[[ "${1:-}" == -- ]] && shift
DOCKER_BUILDKIT=1 docker build \
  -f docker/Dockerfile \
  --build-arg PG_AUTOMERGE_VERSION="$version" \
  --build-arg PG_AUTOMERGE_SOURCE="$source_url" \
  --build-arg PG_AUTOMERGE_REVISION="$revision" \
  -t "$image:$version" -t "$image:dev" \
  "$@" .
echo "$image:$version"
