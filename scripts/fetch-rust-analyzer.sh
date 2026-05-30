#!/usr/bin/env bash
# Download a prebuilt rust-analyzer language server binary.
#
# The Rust provider locates rust-analyzer automatically; by default it looks in
# `.cache/rust-analyzer` at the repo root, which is what this script populates.
# Override the destination with the first argument, or the release with
# RUST_ANALYZER_VERSION (a release tag, or `latest`).
set -euo pipefail

version="${RUST_ANALYZER_VERSION:-latest}"
dest="${1:-.cache/rust-analyzer}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)   triple=x86_64-unknown-linux-gnu ;;
  Linux-aarch64)  triple=aarch64-unknown-linux-gnu ;;
  Darwin-x86_64)  triple=x86_64-apple-darwin ;;
  Darwin-arm64)   triple=aarch64-apple-darwin ;;
  *) echo "unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

base="https://github.com/rust-lang/rust-analyzer/releases"
if [ "$version" = latest ]; then
  url="$base/latest/download/rust-analyzer-$triple.gz"
else
  url="$base/download/$version/rust-analyzer-$triple.gz"
fi

mkdir -p "$dest"
echo "Downloading rust-analyzer ($version, $triple)"
echo "  from $url"
echo "  into $dest"
curl -fSL "$url" -o "$dest/rust-analyzer.gz"
gunzip -f "$dest/rust-analyzer.gz"
chmod +x "$dest/rust-analyzer"

if "$dest/rust-analyzer" --version >/dev/null 2>&1; then
  echo "rust-analyzer installed at $dest/rust-analyzer"
else
  echo "warning: $dest/rust-analyzer did not run" >&2
  exit 1
fi
