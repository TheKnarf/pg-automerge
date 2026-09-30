#!/usr/bin/env bash
# Print the repository URL for the image's org.opencontainers.image.source
# label, from a git remote URL (default: `git remote get-url origin`), or
# nothing. The label is public (`docker image inspect`, every archive and
# pushed image), so credentials never go in:
#   https://user:token@host/owner/repo(.git)  -> https://host/owner/repo
#   ssh://git@host(:port)/owner/repo(.git)    -> https://host/owner/repo
#   git@host:owner/repo(.git)                 -> https://host/owner/repo
# Queries and fragments are dropped; anything else (a local path, file://,
# a URL with unexpected characters) gives nothing.
#
# Usage: oci-source-url.sh [REMOTE_URL]
set -euo pipefail

if (($#)); then url="$1"; else url="$(git -C "$(dirname "$0")/.." remote get-url origin 2>/dev/null || true)"; fi
url="${url%%[?#]*}"
host='' path=''
if [[ "$url" =~ ^https?://([^/]*@)?([^/@]+)/(.+)$ ]]; then
    host="${BASH_REMATCH[2]}"; path="${BASH_REMATCH[3]}"
elif [[ "$url" =~ ^ssh://([^/]*@)?([^/@:]+)(:[0-9]+)?/(.+)$ ]]; then
    host="${BASH_REMATCH[2]}"; path="${BASH_REMATCH[4]}"
elif [[ "$url" =~ ^([^/@:]+@)?([^/@:]+):([^/].*)$ ]]; then
    host="${BASH_REMATCH[2]}"; path="${BASH_REMATCH[3]}"
fi
path="${path%/}"; path="${path%.git}"
out="https://$host/$path"
# A host name (with an optional port for https) and a plain path, or nothing.
if [[ "$out" =~ ^https://[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?(:[0-9]+)?/[A-Za-z0-9._~-]+(/[A-Za-z0-9._~-]+)*$ ]] \
    && [[ "$host" == *.* || "$host" == localhost* ]]; then
    echo "$out"
fi
