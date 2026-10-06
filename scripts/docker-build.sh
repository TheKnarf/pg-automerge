#!/usr/bin/env bash
# Build the Docker images of docker/Dockerfile from one builder stage (the
# extension is compiled once), passing what the Dockerfile cannot read by
# itself: the crate version (checked against Cargo.toml inside the build),
# the base image's Debian release (for the CloudNativePG image's name and
# labels, checked against the base inside the build) and, for the OCI
# labels, the repository URL and commit. Never pushes.
#
#   --full   the default target (runtime): postgres:18 plus the extension,
#            tagged pg-automerge:<version> and pg-automerge:dev
#   --cnpg   the cnpg-extension target: the CloudNativePG image volume
#            extension, tagged pg-automerge-cnpg:<version>-18-<debian>
#   --all    both (the default), the full image first
#
# Prints the tags it built, one per line (the versioned tag of each image).
#
# Env: PG_AUTOMERGE_IMAGE (default pg-automerge), PG_AUTOMERGE_CNPG_IMAGE
# (default pg-automerge-cnpg), PG_AUTOMERGE_PG_IMAGE (the base, default the
# Dockerfile's pinned PG_IMAGE; must be postgres:18-<debian>[@sha256:...];
# for the full image only the Dockerfile's Debian release, whose name its
# tags do not carry; a later release, e.g. forky, for --cnpg. Not bookworm:
# Debian 12 has no rustup package, which the builder installs). Extra
# `docker build` arguments after `--`, passed to each build, e.g.
# `mise run docker-build -- --no-cache`.
set -euo pipefail
cd "$(dirname "$0")/.."
fail() { echo "docker-build.sh: $*" >&2; exit 1; }

targets=(runtime cnpg-extension)
case "${1:-}" in
    --full) targets=(runtime); shift ;;
    --cnpg) targets=(cnpg-extension); shift ;;
    --all) shift ;;
esac
[[ "${1:-}" == -- ]] && shift
[[ "${1:-}" != --full && "${1:-}" != --cnpg && "${1:-}" != --all ]] || fail "--full/--cnpg/--all go before --"

image="${PG_AUTOMERGE_IMAGE:-pg-automerge}"
cnpg_image="${PG_AUTOMERGE_CNPG_IMAGE:-pg-automerge-cnpg}"
version="$(sh scripts/versions.sh | sed -n 's/^CRATE_VERSION=//p')"
# Normalised: no credentials of an https remote in the public label.
source_url="$(bash scripts/oci-source-url.sh)"
revision="$(git rev-parse HEAD 2>/dev/null || true)"
if [[ -n "$revision" ]] && ! git diff --quiet HEAD 2>/dev/null; then revision="$revision-dirty"; fi

# The base and its Debian release (the tag's suffix: postgres:18-trixie).
default_base="$(sed -n 's/^ARG PG_IMAGE=//p' docker/Dockerfile)"
base="${PG_AUTOMERGE_PG_IMAGE:-$default_base}"
[[ "$base" =~ ^([a-z0-9./:-]+/)?postgres:18-([a-z]+)(@sha256:[0-9a-f]{64})?$ ]] \
    || fail "PG_AUTOMERGE_PG_IMAGE must be postgres:18-<debian>[@sha256:<digest>], got '$base'"
debian="${BASH_REMATCH[2]}"
[[ "$default_base" =~ postgres:18-([a-z]+)@ ]] || fail "docker/Dockerfile: no pinned ARG PG_IMAGE=postgres:18-<debian>@..."
# The full image's tags name no Debian release: it stays on the pinned one.
if [[ " ${targets[*]} " == *" runtime "* && "$debian" != "${BASH_REMATCH[1]}" ]]; then
    fail "the full image is built on Debian ${BASH_REMATCH[1]} only; use --cnpg with a postgres:18-$debian base"
fi
args=(
    -f docker/Dockerfile
    --build-arg PG_AUTOMERGE_VERSION="$version"
    --build-arg PG_DEBIAN="$debian"
    --build-arg PG_AUTOMERGE_SOURCE="$source_url"
    --build-arg PG_AUTOMERGE_REVISION="$revision"
)
[[ -z "${PG_AUTOMERGE_PG_IMAGE:-}" ]] || args+=(--build-arg PG_IMAGE="$base")

built=()
for target in "${targets[@]}"; do
    if [[ "$target" == runtime ]]; then
        tags=(-t "$image:$version" -t "$image:dev"); built+=("$image:$version")
    else
        tags=(-t "$cnpg_image:$version-18-$debian"); built+=("$cnpg_image:$version-18-$debian")
    fi
    DOCKER_BUILDKIT=1 docker build "${args[@]}" --target "$target" "${tags[@]}" "$@" .
done
printf '%s\n' "${built[@]}"
