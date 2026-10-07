#!/usr/bin/env bash
# Filter a GitHub release's notes (stdin) into the same notes with the
# section this repository's release workflow adds (stdout): docker pull
# commands for both images, a CloudNativePG Cluster/Database snippet and
# the release assets. The section sits between two HTML comment markers
# and is replaced, never repeated, so running it again on its own output
# gives the same notes (re-running the release workflow is safe). The rest
# of the notes, as written in "Draft a new release", stays as it was.
#
# Usage: release-notes.sh VERSION IMAGE CNPG_IMAGE DEBIAN REVISION <notes >new-notes
#   IMAGE, CNPG_IMAGE: the repositories without a tag
#   (ghcr.io/<owner>/pg-automerge, ghcr.io/<owner>/pg-automerge-cnpg).
set -euo pipefail
fail() { echo "release-notes.sh: $*" >&2; exit 1; }
(($# == 5)) || fail "usage: release-notes.sh VERSION IMAGE CNPG_IMAGE DEBIAN REVISION <notes >new-notes"
version="$1" image="$2" cnpg_image="$3" debian="$4" revision="$5"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?$ ]] || fail "bad version '$version'"
[[ "$debian" =~ ^[a-z]+$ ]] || fail "bad Debian release '$debian'"

begin='<!-- pg-automerge-release:begin (written by .github/workflows/release.yml; replaced on every run) -->'
end='<!-- pg-automerge-release:end -->'

# The notes without an earlier section (matched by the markers' prefixes),
# without trailing blank lines; CRLF line ends (the web editor's) are
# normalised.
notes="$(tr -d '\r' | awk '
    index($0, "<!-- pg-automerge-release:begin") == 1 { skip = 1 }
    !skip { print }
    index($0, "<!-- pg-automerge-release:end") == 1 { skip = 0 }
')"
# Command substitution has dropped the trailing newlines; drop trailing
# blank lines with spaces too.
while [[ "$notes" == *$'\n' || "$notes" =~ $'\n'[[:blank:]]*$ ]]; do notes="${notes%$'\n'*}"; done
[[ "$notes" =~ ^[[:space:]]*$ ]] && notes=''

cnpg_ref="$cnpg_image:$version-18-$debian"
package="pg_automerge-$version-pg18-linux-amd64.tar.gz"
fence='```'
section="$(cat <<EOF
$begin
## Docker images

Built and tested by the release workflow from $revision, for
linux/amd64 and linux/arm64:

${fence}sh
docker pull $image:$version
docker pull $cnpg_ref   # CloudNativePG extension image (files only)
${fence}

The first is the official \`postgres:18\` image plus the extension (see
the Docker guide). The second is mounted by
[CloudNativePG](https://cloudnative-pg.io) 1.27 or later into its own
PostgreSQL 18 $debian image (Kubernetes image volumes):

${fence}yaml
apiVersion: postgresql.cnpg.io/v1
kind: Cluster
metadata:
  name: automerge
spec:
  instances: 3
  imageName: ghcr.io/cloudnative-pg/postgresql:18-minimal-$debian
  storage:
    size: 10Gi
  postgresql:
    extensions:
      - name: pg-automerge
        image:
          reference: $cnpg_ref
---
apiVersion: postgresql.cnpg.io/v1
kind: Database
metadata:
  name: automerge-app
spec:
  name: app
  owner: app
  cluster:
    name: automerge
  extensions:
    - name: pg_automerge
      version: "$version"
${fence}

## Assets

- \`$package\`: the extension built against the PGDG PostgreSQL 18 on
  Ubuntu 24.04 (amd64); \`sudo tar -C / -xzf $package\` puts it into
  \`/usr/lib/postgresql/18/lib\` and \`/usr/share/postgresql/18/extension\`.
- \`pg-automerge-$version-linux-<arch>.tar.gz\`,
  \`pg-automerge-cnpg-$version-18-$debian-linux-<arch>.tar.gz\`: the
  pushed images as \`docker save\` archives (\`docker load -i <file>\`).
- \`SHA256SUMS\`: \`sha256sum -c SHA256SUMS\`.
$end
EOF
)"

if [[ -n "$notes" ]]; then printf '%s\n\n%s\n' "$notes" "$section"; else printf '%s\n' "$section"; fi
