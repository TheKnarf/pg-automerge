#!/usr/bin/env bash
# Push image archives made by scripts/docker-archive.sh (one per
# architecture) to a registry as one multi-architecture image. Used by the
# CI publish job, which runs only on version tags and only once a registry
# is configured (see README.md, "Publishing the image"). Nothing in this
# repository calls it otherwise.
#
# For each archive: `docker load`, check that the image's version label is
# Cargo.toml's version and its architecture the one in the file name, tag
# it REPOSITORY:<version>-<arch> and push that. Then
# `docker buildx imagetools create` makes REPOSITORY:<version> (and
# REPOSITORY:latest, unless PG_AUTOMERGE_PUSH_LATEST=0) an index of the
# per-architecture images. The caller must be logged in to the registry.
#
# --dry-run: load and check everything, print the push and imagetools
# commands instead of running them, and remove the tags it made.
#
# Usage: docker-push.sh [--dry-run] REPOSITORY ARCHIVE...
#   REPOSITORY: fully qualified, registry host first, lower case, e.g.
#   ghcr.io/you/pg-automerge or docker.io/you/pg-automerge.
set -euo pipefail
cd "$(dirname "$0")/.."
fail() { echo "docker-push.sh: $*" >&2; exit 1; }

dry_run=0
if [[ "${1:-}" == --dry-run ]]; then dry_run=1; shift; fi
(($# >= 2)) || fail "usage: docker-push.sh [--dry-run] REPOSITORY ARCHIVE..."
repo="$1"; shift
# A registry host (with a dot or port, or localhost) and a path: never a
# bare name that Docker would silently send to Docker Hub.
[[ "$repo" =~ ^([a-z0-9-]+\.[a-z0-9.-]+|[a-z0-9.-]+:[0-9]+|localhost)/[a-z0-9]+([._/-][a-z0-9]+)*$ ]] \
    || fail "REPOSITORY must be registry-host/path in lower case (e.g. ghcr.io/you/pg-automerge), got '$repo'"

version="$(sh scripts/versions.sh | sed -n 's/^CRATE_VERSION=//p')"
run() {
    if ((dry_run)); then printf '+ %s\n' "$*"; else "$@"; fi
}

tags=()
cleanup() { if ((dry_run)) && ((${#tags[@]})); then docker rmi "${tags[@]}" >/dev/null 2>&1 || true; fi; }
trap cleanup EXIT

seen=" "
for archive in "$@"; do
    name="$(basename "$archive")"
    [[ "$name" =~ ^pg-automerge-([0-9A-Za-z.+-]+)-linux-([a-z0-9]+)\.tar\.gz$ ]] \
        || fail "$archive: not a pg-automerge-<version>-linux-<arch>.tar.gz archive"
    [[ "${BASH_REMATCH[1]}" == "$version" ]] || fail "$archive: version ${BASH_REMATCH[1]}, Cargo.toml says $version"
    arch="${BASH_REMATCH[2]}"
    [[ "$seen" != *" $arch "* ]] || fail "two archives for $arch"
    seen+="$arch "

    loaded="$(docker load -i "$archive" | sed -n 's/^Loaded image: //p' | tail -1)"
    [[ -n "$loaded" ]] || fail "$archive: docker load reported no image"
    label="$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.version"}}' "$loaded")"
    [[ "$label" == "$version" ]] || fail "$archive: image version label '$label', expected $version"
    image_arch="$(docker image inspect -f '{{.Architecture}}' "$loaded")"
    [[ "$image_arch" == "$arch" ]] || fail "$archive: image architecture $image_arch, file name says $arch"

    tag="$repo:$version-$arch"
    docker tag "$loaded" "$tag"
    tags+=("$tag")
    run docker push "$tag"
done

index=(-t "$repo:$version")
[[ "${PG_AUTOMERGE_PUSH_LATEST:-1}" == 0 ]] || index+=(-t "$repo:latest")
run docker buildx imagetools create "${index[@]}" "${tags[@]}"
