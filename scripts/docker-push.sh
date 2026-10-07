#!/usr/bin/env bash
# Push image archives made by scripts/docker-archive.sh (one per
# architecture) to a registry as one multi-architecture image. Used by the
# publish job of .github/workflows/release.yml, which runs for a published
# GitHub release after every test job passed and pushes to ghcr.io with the
# workflow's GITHUB_TOKEN (see docs/src/pages/operations/releasing.mdx);
# the caller must be logged in to the registry.
#
# For each archive: `docker load`, check that the image's version label is
# Cargo.toml's version and its architecture the one in the file name, tag
# it REPOSITORY:<version>-<arch> and push that. Prints `loaded ARCHIVE
# IMAGE-ID` for each. The local tags the archive carries (e.g.
# pg-automerge:<version>) are put back as they were before the load, so
# loading the arm64 archive does not repoint your amd64 pg-automerge tag. Then
# `docker buildx imagetools create` makes REPOSITORY:<version> an index of
# the per-architecture images, annotated with the images' OCI source,
# revision and version labels (which must be the same in every archive).
#
# --cnpg: the archives are CloudNativePG extension images
# (pg-automerge-cnpg-<version>-18-<debian>-linux-<arch>.tar.gz), each also
# checked for its io.cloudnativepg.image.base.os label, tagged
# REPOSITORY:<version>-18-<debian>-<arch>, and indexed per Debian release as
# REPOSITORY:<version>-18-<debian> (CNPG's tag convention; never latest).
# --latest: also point REPOSITORY:latest at the index (not with --cnpg).
# --arch ARCH (repeatable): the archives must cover exactly these
#   architectures (for each Debian release, with --cnpg); release.yml
#   passes --arch amd64 --arch arm64.
# --revision SHA, --source URL: every image's org.opencontainers.image.
#   revision / .source label must be exactly this (release.yml: the tag's
#   commit and the repository).
# --dry-run: load and check everything, print the push and imagetools
#   commands instead of running them, and remove the tags it made (and an
#   image the load added that no tag refers to any more).
#
# Usage: docker-push.sh [--dry-run] [--cnpg] [--latest] [--arch ARCH]...
#                       [--revision SHA] [--source URL] REPOSITORY ARCHIVE...
#   REPOSITORY: fully qualified, registry host first, lower case, e.g.
#   ghcr.io/you/pg-automerge (with --cnpg, ghcr.io/you/pg-automerge-cnpg).
set -euo pipefail
cd "$(dirname "$0")/.."
fail() { echo "docker-push.sh: $*" >&2; exit 1; }

dry_run=0 cnpg=0 latest=0 want_revision='' want_source=''
want_arches=()
while (($#)); do
    case "$1" in
        --dry-run) dry_run=1; shift ;;
        --cnpg) cnpg=1; shift ;;
        --latest) latest=1; shift ;;
        --arch) [[ "${2:-}" =~ ^[a-z0-9]+$ ]] || fail "--arch needs an architecture"; want_arches+=("$2"); shift 2 ;;
        --revision) [[ "${2:-}" =~ ^[0-9a-f]{40}$ ]] || fail "--revision needs a full commit SHA"; want_revision="$2"; shift 2 ;;
        --source) [[ "${2:-}" =~ ^https://[^@[:space:]]+$ ]] || fail "--source needs an https URL"; want_source="$2"; shift 2 ;;
        --*) fail "unknown option $1" ;;
        *) break ;;
    esac
done
if ((cnpg && latest)); then fail "--latest is not for the CNPG extension image (CNPG's tags have no latest)"; fi
usage="usage: docker-push.sh [--dry-run] [--cnpg] [--latest] [--arch ARCH]... [--revision SHA] [--source URL] REPOSITORY ARCHIVE..."
(($# >= 2)) || fail "$usage"
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
declare -A restore=()   # local tag -> its image ID before the loads ("" = none)
new_images=()           # image IDs the loads added
cleanup() {
    local status=$? tag id
    if ((dry_run)) && ((${#tags[@]})); then docker rmi "${tags[@]}" >/dev/null 2>&1 || true; fi
    for tag in "${!restore[@]}"; do
        id="$(docker image inspect -f '{{.Id}}' "$tag" 2>/dev/null || true)"
        [[ "$id" == "${restore[$tag]}" ]] && continue
        if [[ -n "${restore[$tag]}" ]]; then
            docker tag "${restore[$tag]}" "$tag" || { echo "docker-push.sh: could not restore $tag to ${restore[$tag]}" >&2; status=1; }
        else
            docker rmi "$tag" >/dev/null || { echo "docker-push.sh: could not remove the tag $tag the load made" >&2; status=1; }
        fi
    done
    if ((dry_run)); then
        for id in "${new_images[@]}"; do
            [[ -z "$(docker image inspect -f '{{join .RepoTags " "}}' "$id" 2>/dev/null)" ]] \
                && docker rmi "$id" >/dev/null 2>&1 || true
        done
    fi
    exit "$status"
}
trap cleanup EXIT

seen=" "
declare -A index_tags=()   # index tag -> the per-arch tags it is made of
declare -A index_arches=() # index tag -> the architectures in it
source_label='' revision_label='' first=1
for archive in "$@"; do
    name="$(basename "$archive")"
    debian=''
    if ((cnpg)); then
        [[ "$name" =~ ^pg-automerge-cnpg-([0-9A-Za-z.+-]+)-18-([a-z]+)-linux-([a-z0-9]+)\.tar\.gz$ ]] \
            || fail "$archive: not a pg-automerge-cnpg-<version>-18-<debian>-linux-<arch>.tar.gz archive"
        debian="${BASH_REMATCH[2]}"; arch="${BASH_REMATCH[3]}"
    else
        [[ "$name" != pg-automerge-cnpg-* ]] || fail "$archive: a CNPG extension image archive (push it with --cnpg)"
        [[ "$name" =~ ^pg-automerge-([0-9A-Za-z.+-]+)-linux-([a-z0-9]+)\.tar\.gz$ ]] \
            || fail "$archive: not a pg-automerge-<version>-linux-<arch>.tar.gz archive"
        arch="${BASH_REMATCH[2]}"
    fi
    [[ "${BASH_REMATCH[1]}" == "$version" ]] || fail "$archive: version ${BASH_REMATCH[1]}, Cargo.toml says $version"
    key="${debian:+$debian/}$arch"
    [[ "$seen" != *" $key "* ]] || fail "two archives for $key"
    seen+="$key "

    # The tags in the archive, and what they point to now, to put back.
    archive_tags="$(tar -xzOf "$archive" manifest.json | jq -r '.[].RepoTags[]?')" \
        || fail "$archive: no manifest.json (not a docker save archive)"
    [[ -n "$archive_tags" ]] || fail "$archive: the image in it has no tag"
    for t in $archive_tags; do
        [[ -v "restore[$t]" ]] || restore[$t]="$(docker image inspect -f '{{.Id}}' "$t" 2>/dev/null || true)"
    done
    before="$(docker image ls -aq --no-trunc)"
    out="$(docker load -i "$archive")"
    loaded="$(sed -n 's/^Loaded image: //p' <<<"$out" | tail -1)"
    [[ -n "$loaded" ]] || fail "$archive: docker load reported no image"
    loaded="$(docker image inspect -f '{{.Id}}' "$loaded")"
    grep -qxF "$loaded" <<<"$before" || new_images+=("$loaded")
    echo "loaded $archive $loaded"
    label="$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.version"}}' "$loaded")"
    [[ "$label" == "$version" ]] || fail "$archive: image version label '$label', expected $version"
    image_arch="$(docker image inspect -f '{{.Architecture}}' "$loaded")"
    [[ "$image_arch" == "$arch" ]] || fail "$archive: image architecture $image_arch, file name says $arch"
    image_debian="$(docker image inspect -f '{{index .Config.Labels "io.cloudnativepg.image.base.os"}}' "$loaded")"
    [[ "$image_debian" == "$debian" ]] || fail "$archive: image io.cloudnativepg.image.base.os label '$image_debian', expected '$debian'"

    # One commit and repository for every image of the release.
    src="$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.source"}}' "$loaded")"
    rev="$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$loaded")"
    [[ -z "$want_source" || "$src" == "$want_source" ]] || fail "$archive: image source label '$src', expected $want_source"
    [[ -z "$want_revision" || "$rev" == "$want_revision" ]] || fail "$archive: image revision label '$rev', expected $want_revision"
    if ((first)); then
        source_label="$src" revision_label="$rev" first=0
    elif [[ "$src" != "$source_label" || "$rev" != "$revision_label" ]]; then
        fail "$archive: source/revision labels '$src' '$rev' differ from the first archive's ('$source_label' '$revision_label')"
    fi

    if ((cnpg)); then index_tag="$repo:$version-18-$debian"; else index_tag="$repo:$version"; fi
    tag="$index_tag-$arch"
    docker tag "$loaded" "$tag"
    tags+=("$tag")
    index_tags[$index_tag]+=" $tag"
    index_arches[$index_tag]+=" $arch"
done

# Every index must hold exactly the required architectures (checked for
# all of them before anything is pushed).
if ((${#want_arches[@]})); then
    want="$(printf '%s\n' "${want_arches[@]}" | sort -u | tr '\n' ' ')"
    for index_tag in "${!index_arches[@]}"; do
        # shellcheck disable=SC2086 # a word list of architectures
        got="$(printf '%s\n' ${index_arches[$index_tag]} | sort -u | tr '\n' ' ')"
        [[ "$got" == "$want" ]] || fail "$index_tag: archives for ${got% }, required ${want% }"
    done
fi

for tag in "${tags[@]}"; do run docker push "$tag"; done

# The index carries the images' labels as annotations (what registries
# such as ghcr.io show for a multi-architecture image).
annotations=()
[[ -z "$source_label" ]] || annotations+=(--annotation "index:org.opencontainers.image.source=$source_label")
[[ -z "$revision_label" ]] || annotations+=(--annotation "index:org.opencontainers.image.revision=$revision_label")
annotations+=(--annotation "index:org.opencontainers.image.version=$version")

for index_tag in $(printf '%s\n' "${!index_tags[@]}" | sort); do
    index=(-t "$index_tag")
    if ((latest)); then index+=(-t "$repo:latest"); fi
    # shellcheck disable=SC2086 # a word list of tags
    run docker buildx imagetools create "${annotations[@]}" "${index[@]}" ${index_tags[$index_tag]}
done
