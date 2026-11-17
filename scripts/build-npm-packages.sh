#!/usr/bin/env bash
set -euo pipefail

# Packages the compiled release binary into a platform-specific npm package directory
# matching `npm/package.json` optionalDependencies.
VERSION="${1:-0.1.0}"
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "${ARCH}" in
  x86_64|amd64) ARCH="x64" ;;
  arm64|aarch64) ARCH="arm64" ;;
esac

PKG_KEY="${OS}-${ARCH}"
OUT_DIR="dist/npm-${PKG_KEY}"
mkdir -p "${OUT_DIR}/bin"

cargo build --release
cp target/release/abox "${OUT_DIR}/bin/abox"
chmod +x "${OUT_DIR}/bin/abox"

cat > "${OUT_DIR}/package.json" <<EOF
{
  "name": "@agentbox/${PKG_KEY}",
  "version": "${VERSION}",
  "license": "Apache-2.0",
  "os": ["${OS}"],
  "cpu": ["${ARCH}"],
  "files": ["bin/abox"]
}
EOF

echo "Packaged @agentbox/${PKG_KEY} -> ${OUT_DIR}"
