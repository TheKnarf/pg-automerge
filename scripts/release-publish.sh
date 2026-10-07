#!/usr/bin/env bash
# Publish a GitHub release's tested build: the publish job of
# .github/workflows/release.yml runs it once every test job passed, logged
# in to ghcr.io with the workflow's GITHUB_TOKEN (see
# docs/src/pages/operations/releasing.mdx). DIST holds the workflow's
# artifacts: the package tarball (pg_automerge-pg18.tar.gz) and the image
# archives of both images for amd64 and arm64 (scripts/docker-archive.sh).
#
#   1. Checks: TAG is v<Cargo.toml version>, REVISION the checked-out commit,
#      every archive there (scripts/docker-push.sh --dry-run of both image
#      sets: version and architecture labels, amd64 and arm64 both present,
#      no duplicates, source and revision labels the repository's and the
#      tag commit's) before anything is pushed.
#   2. Pushes ghcr.io/<owner>/pg-automerge-cnpg:<version>-18-<debian>, then
#      ghcr.io/<owner>/pg-automerge:<version> (and :latest with --latest),
#      each a multi-architecture index (scripts/docker-push.sh).
#   3. Uploads the release assets with `gh release upload --clobber`: the
#      package as pg_automerge-<version>-pg18-linux-amd64.tar.gz, the four
#      image archives and SHA256SUMS.
#   4. Replaces its section of the release notes (scripts/release-notes.sh)
#      with `gh release edit`.
# Every step is idempotent (the same bytes, the same tags, --clobber, the
# section replaced), so running it again for the same release is safe.
#
# --dry-run: steps 1 and 2 as docker-push.sh --dry-run (load and check,
# print the pushes and imagetools commands), the assets and SHA256SUMS
# built in DIST/release-assets, and the gh commands and the new notes
# printed instead of run (the current notes read from --notes-file, or
# empty). Needs no registry login and no GitHub token.
#
# Usage: release-publish.sh [--dry-run] [--latest] [--notes-file FILE]
#            TAG REVISION REPOSITORY DIST
#   REPOSITORY: owner/name on GitHub (GITHUB_REPOSITORY).
set -euo pipefail
cd "$(dirname "$0")/.."
fail() { echo "release-publish.sh: $*" >&2; exit 1; }

dry_run=0 latest=() notes_file=''
while (($#)); do
    case "$1" in
        --dry-run) dry_run=1; shift ;;
        --latest) latest=(--latest); shift ;;
        --notes-file) notes_file="${2:-}"; [[ -f "$notes_file" ]] || fail "--notes-file needs a file"; shift 2 ;;
        --*) fail "unknown option $1" ;;
        *) break ;;
    esac
done
(($# == 4)) || fail "usage: release-publish.sh [--dry-run] [--latest] [--notes-file FILE] TAG REVISION REPOSITORY DIST"
tag="$1" revision="$2" repository="$3" dist="$4"
run() {
    if ((dry_run)); then printf '+ %s\n' "$*"; else "$@"; fi
}

version="$(sh scripts/versions.sh | sed -n 's/^CRATE_VERSION=//p')"
[[ "$tag" == "v$version" ]] || fail "the release tag is '$tag', but Cargo.toml's version is $version: the tag must be v$version"
[[ "$revision" =~ ^[0-9a-f]{40}$ ]] || fail "REVISION must be a full commit SHA, got '$revision'"
head="$(git rev-parse HEAD 2>/dev/null || true)"
[[ "$head" == "$revision" ]] || ((dry_run)) || fail "the checkout is at '$head', not the release commit $revision"
[[ "$repository" =~ ^[A-Za-z0-9-]+/[A-Za-z0-9._-]+$ ]] || fail "REPOSITORY must be owner/name, got '$repository'"
[[ -d "$dist" ]] || fail "no directory $dist"

owner="${repository%%/*}"; owner="${owner,,}"
image="ghcr.io/$owner/pg-automerge"
cnpg_image="ghcr.io/$owner/pg-automerge-cnpg"
source_url="${GITHUB_SERVER_URL:-https://github.com}/$repository"

package="$dist/pg_automerge-pg18.tar.gz"
[[ -f "$package" ]] || fail "$package missing (the ci job's package artifact)"
shopt -s nullglob
full=("$dist"/pg-automerge-"$version"-linux-*.tar.gz)
cnpg=("$dist"/pg-automerge-cnpg-"$version"-18-*-linux-*.tar.gz)
shopt -u nullglob
((${#full[@]})) || fail "no pg-automerge-$version-linux-<arch>.tar.gz in $dist"
((${#cnpg[@]})) || fail "no pg-automerge-cnpg-$version-18-<debian>-linux-<arch>.tar.gz in $dist"
debians="$(for f in "${cnpg[@]}"; do basename "$f"; done | sed -E "s/^pg-automerge-cnpg-$version-18-([a-z]+)-linux-.*/\\1/" | sort -u)"
[[ "$debians" =~ ^[a-z]+$ ]] || fail "the CNPG archives must be of one Debian release, got: $(tr '\n' ' ' <<<"$debians")"
debian="$debians"

check=(--arch amd64 --arch arm64 --revision "$revision" --source "$source_url")
echo "==> check both image sets (nothing pushed yet)"
bash scripts/docker-push.sh --dry-run --cnpg "${check[@]}" "$cnpg_image" "${cnpg[@]}"
bash scripts/docker-push.sh --dry-run "${check[@]}" "${latest[@]}" "$image" "${full[@]}"
if ((!dry_run)); then
    # The CNPG image first and the full image last, so :latest moves only
    # once everything else is up.
    echo "==> push $cnpg_image:$version-18-$debian"
    bash scripts/docker-push.sh --cnpg "${check[@]}" "$cnpg_image" "${cnpg[@]}"
    echo "==> push $image:$version${latest[0]:+ and :latest}"
    bash scripts/docker-push.sh "${check[@]}" "${latest[@]}" "$image" "${full[@]}"
    docker buildx imagetools inspect "$image:$version"
    docker buildx imagetools inspect "$cnpg_image:$version-18-$debian"
fi

echo "==> release assets"
assets="$dist/release-assets"
rm -rf "$assets"; mkdir -p "$assets"
cp "$package" "$assets/pg_automerge-$version-pg18-linux-amd64.tar.gz"
cp "${full[@]}" "${cnpg[@]}" "$assets/"
(cd "$assets" && sha256sum -- *.tar.gz >SHA256SUMS && cat SHA256SUMS)
run gh release upload "$tag" "$assets"/* --clobber --repo "$repository"

echo "==> release notes"
if ((dry_run)); then
    if [[ -n "$notes_file" ]]; then cat "$notes_file"; fi >"$dist/notes-before.md"
else
    gh release view "$tag" --repo "$repository" --json body --jq .body >"$dist/notes-before.md"
fi
bash scripts/release-notes.sh "$version" "$image" "$cnpg_image" "$debian" "$revision" \
    <"$dist/notes-before.md" >"$dist/notes.md"
rm "$dist/notes-before.md"
if ((dry_run)); then cat "$dist/notes.md"; fi
run gh release edit "$tag" --repo "$repository" --notes-file "$dist/notes.md"
