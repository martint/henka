#!/usr/bin/env bash
# Build the jdtls delegate-command bundle against the local jdtls distribution.
#
# This is a small OSGi bundle compiled against jdtls's own jars (offline), so it
# is built with javac+jar rather than Maven — there is no Maven project here and
# resolving an Eclipse target platform would only add friction.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
jdtls="${JDTLS_HOME:-$here/../.cache/jdtls}"
plugins="$jdtls/plugins"
if [ ! -d "$plugins" ]; then
  echo "jdtls plugins not found at $plugins; run scripts/fetch-jdtls.sh first" >&2
  exit 1
fi

cp=$(ls "$plugins"/*.jar | tr '\n' ':')
out="$here/build"
rm -rf "$out"
mkdir -p "$out/classes"

javac --release 21 -cp "$cp" -d "$out/classes" $(find "$here/src" -name '*.java')

jar="$here/henka-jdtls-bundle.jar"
( cd "$here" && jar cfm "$jar" META-INF/MANIFEST.MF -C "$out/classes" . plugin.xml )
echo "built $jar"
