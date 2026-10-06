#!/usr/bin/env bash
# Save a built image as a gzipped `docker save` archive, the tag CI job's
# workflow artifact (read back with `docker load -i`; scripts/docker-push.sh
# pushes such archives). Prints the path. The archive is named
#   pg-automerge-<version>-linux-<arch>.tar.gz                for the full image
#   pg-automerge-cnpg-<version>-18-<debian>-linux-<arch>.tar.gz  for the CNPG
#                                                     extension image
# (told apart by its io.cloudnativepg.image.base.os label). The version is
# the image's OCI version label, which must equal Cargo.toml's; the
# architecture is the image's.
#
# Usage: docker-archive.sh [IMAGE (default pg-automerge:<Cargo.toml version>)] [OUTDIR (default .)]
set -euo pipefail
cd "$(dirname "$0")/.."
fail() { echo "docker-archive.sh: $*" >&2; exit 1; }

version="$(sh scripts/versions.sh | sed -n 's/^CRATE_VERSION=//p')"
image="${1:-pg-automerge:$version}"
outdir="${2:-.}"

docker image inspect "$image" >/dev/null 2>&1 || fail "no image $image (mise run docker-build)"
label() { docker image inspect -f "{{index .Config.Labels \"$1\"}}" "$image"; }
[[ "$(label org.opencontainers.image.version)" == "$version" ]] \
    || fail "$image is version '$(label org.opencontainers.image.version)', Cargo.toml says $version"
arch="$(docker image inspect -f '{{.Architecture}}' "$image")"
[[ "$arch" =~ ^[a-z0-9]+$ ]] || fail "$image: unexpected architecture '$arch'"
debian="$(label io.cloudnativepg.image.base.os)"
if [[ -z "$debian" ]]; then
    name="pg-automerge-$version"
else
    [[ "$debian" =~ ^[a-z]+$ && "$(label io.cloudnativepg.image.base.pgmajor)" == 18 ]] \
        || fail "$image: unexpected CNPG labels (os '$debian', pgmajor '$(label io.cloudnativepg.image.base.pgmajor)')"
    name="pg-automerge-cnpg-$version-18-$debian"
fi

mkdir -p "$outdir"
file="$outdir/$name-linux-$arch.tar.gz"
# Written under a temporary name so a failed save leaves no archive behind.
# gzip -1: `docker save` dominates the time; -6 would add about 30 s on a
# slow host for 10% less (177 MB instead of 161 MB).
trap 'rm -f "$file.part"' EXIT
docker save "$image" | gzip -1 >"$file.part"
mv "$file.part" "$file"
echo "$file"
