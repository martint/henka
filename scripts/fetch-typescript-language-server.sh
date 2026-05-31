#!/usr/bin/env bash
# Install the typescript-language-server (and the TypeScript it wraps) into a
# local prefix.
#
# The TypeScript/JavaScript provider locates the server automatically; by
# default it looks in `.cache/typescript-language-server` at the repo root,
# which is what this script populates. Override the destination with the first
# argument, or the versions with TYPESCRIPT_LANGUAGE_SERVER_VERSION /
# TYPESCRIPT_VERSION.
set -euo pipefail

dest="${1:-.cache/typescript-language-server}"
ls_version="${TYPESCRIPT_LANGUAGE_SERVER_VERSION:-latest}"
ts_version="${TYPESCRIPT_VERSION:-latest}"

if ! command -v npm >/dev/null 2>&1; then
  echo "npm is required to fetch typescript-language-server (Node toolchain)" >&2
  exit 1
fi

mkdir -p "$dest"
echo "Installing typescript-language-server ($ls_version) + typescript ($ts_version)"
echo "  into $dest"
npm install --prefix "$dest" --no-save --no-fund --no-audit \
  "typescript-language-server@$ls_version" "typescript@$ts_version"

bin="$dest/node_modules/.bin/typescript-language-server"
if [ -x "$bin" ]; then
  echo "typescript-language-server installed at $bin"
else
  echo "warning: $bin not found after install" >&2
  exit 1
fi
