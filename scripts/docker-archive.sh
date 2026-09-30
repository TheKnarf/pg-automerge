#!/usr/bin/env bash
# Save a built image as a gzipped `docker save` archive named
# pg-automerge-<version>-linux-<arch>.tar.gz, the tag CI job's workflow
# artifact (read back with `docker load -i`; scripts/docker-push.sh pushes
# such archives). The version is the image's OCI version label, which must
# equal Cargo.toml's; the architecture is the image's. Prints the path.
#
# Usage: docker-archive.sh [IMAGE (default pg-automerge:<Cargo.toml version>)] [OUTDIR (default .)]
set -euo pipefail
cd "$(dirname "$0")/.."
fail() { echo "docker-archive.sh: $*" >&2; exit 1; }

version="$(sh scripts/versions.sh | sed -n 's/^CRATE_VERSION=//p')"
image="${1:-pg-automerge:$version}"
outdir="${2:-.}"

docker image inspect "$image" >/dev/null 2>&1 || fail "no image $image (mise run docker-build)"
label="$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.version"}}' "$image")"
[[ "$label" == "$version" ]] || fail "$image is version '$label', Cargo.toml says $version"
arch="$(docker image inspect -f '{{.Architecture}}' "$image")"
[[ "$arch" =~ ^[a-z0-9]+$ ]] || fail "$image: unexpected architecture '$arch'"

mkdir -p "$outdir"
file="$outdir/pg-automerge-$version-linux-$arch.tar.gz"
# Written under a temporary name so a failed save leaves no archive behind.
# gzip -1: `docker save` dominates the time; -6 would add about 30 s on a
# slow host for 10% less (177 MB instead of 161 MB).
trap 'rm -f "$file.part"' EXIT
docker save "$image" | gzip -1 >"$file.part"
mv "$file.part" "$file"
echo "$file"
