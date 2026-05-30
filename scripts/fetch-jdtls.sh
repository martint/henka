#!/usr/bin/env bash
# Download and extract an Eclipse JDT language server distribution.
#
# The Java provider locates jdtls automatically; by default it looks in
# `.cache/jdtls` at the repo root, which is what this script populates. Override
# the destination with the first argument, or the version with JDTLS_VERSION.
set -euo pipefail

version="${JDTLS_VERSION:-latest}"
dest="${1:-.cache/jdtls}"
url="https://download.eclipse.org/jdtls/snapshots/jdt-language-server-${version}.tar.gz"

mkdir -p "$dest"
echo "Downloading jdtls ($version)"
echo "  from $url"
echo "  into $dest"
curl -fSL "$url" -o "$dest/jdtls.tar.gz"
tar -xzf "$dest/jdtls.tar.gz" -C "$dest"
rm -f "$dest/jdtls.tar.gz"

if compgen -G "$dest/plugins/org.eclipse.equinox.launcher_*.jar" > /dev/null; then
  echo "jdtls installed at $dest"
else
  echo "warning: launcher jar not found under $dest/plugins" >&2
  exit 1
fi

# Build the delegate-command bundle (parameterized refactorings) against the
# freshly fetched jdtls, when its build script is present.
bundle_build="$(dirname "$0")/../jdtls-bundle/build.sh"
if [ -f "$bundle_build" ]; then
  echo "building jdtls delegate-command bundle"
  JDTLS_HOME="$dest" bash "$bundle_build"
fi
