#!/usr/bin/env bash
# Stage every published release's client as the Worker's static assets, so each deploy
# keeps all released versions at /v/<version>/ (ADR 0002), not only the current one.
#
#   GH_TOKEN=... deploy/stage-releases.sh [assets dir]   (default castcorder/worker/assets)
#
# release.yml attaches castcorder-client-<version>.tar.gz (the trunk build, files at the
# archive root) to each release; this unpacks it at <assets dir>/v/<version>/. Drafts are
# skipped (nothing links to them yet). A published release without that asset is an error:
# its hosts hand out links to /v/<version>/, which must keep working.
set -euo pipefail

assets="${1:-castcorder/worker/assets}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

mapfile -t tags < <(gh release list --exclude-drafts --limit 1000 --json tagName --jq '.[].tagName')
if [ "${#tags[@]}" -eq 0 ]; then
  echo "no published releases yet: only this build's client is deployed"
  exit 0
fi

for tag in "${tags[@]}"; do
  rm -rf "${work:?}"/*
  if ! gh release download "$tag" --pattern 'castcorder-client-*.tar.gz' --dir "$work" 2>/dev/null; then
    echo "::error::release $tag has no castcorder-client-<version>.tar.gz asset; attach one (release.yml builds it)"
    exit 1
  fi
  archive="$(find "$work" -maxdepth 1 -name 'castcorder-client-*.tar.gz' | head -n 1)"
  version="$(basename "$archive" .tar.gz)"
  version="${version#castcorder-client-}"
  if ! [[ "$version" =~ ^[0-9A-Za-z.+-]+$ ]]; then
    echo "::error::release $tag: bad client version '$version' in the asset name"
    exit 1
  fi
  dest="$assets/v/$version"
  rm -rf "$dest"
  mkdir -p "$dest"
  tar -xzf "$archive" -C "$dest"
  if [ ! -f "$dest/index.html" ]; then
    echo "::error::release $tag: the client archive has no index.html at its root"
    exit 1
  fi
  echo "staged release $tag client at /v/$version/"
done
