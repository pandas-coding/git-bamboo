#!/usr/bin/env bash
# Builds platform-specific VSIX packages for Git Bamboo (plan step 13):
#   1. cargo build --release per target triple
#   2. copy the engine binary into editors/vscode/server/
#   3. vsce package --target <vsce-target>
#
# Requires the Rust targets installed (rustup target add <triple>) and, for
# cross-compilation, an appropriate linker (e.g. cargo-xwin for
# x86_64-pc-windows-msvc from Linux, or osxcross for apple targets).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXT_DIR="$ROOT/editors/vscode"
ENGINE_NAME="bamboo-engine"
OUT_DIR="$ROOT/dist"

# rust target triple : vsce target : binary suffix
TARGETS=(
  "x86_64-unknown-linux-gnu:linux-x64:"
  "x86_64-pc-windows-msvc:win32-x64:.exe"
  "aarch64-apple-darwin:darwin-arm64:"
)

# Build the extension bundle (out/) if it is not present yet.
if [ ! -f "$EXT_DIR/out/extension.js" ]; then
  echo "==> Building extension bundle"
  (cd "$EXT_DIR" && npm run build)
fi

VERSION="$(cd "$EXT_DIR" && node -p "require('./package.json').version")"
mkdir -p "$OUT_DIR" "$EXT_DIR/server"

for entry in "${TARGETS[@]}"; do
  TRIPLE="${entry%%:*}"
  REST="${entry#*:}"
  VSCE_TARGET="${REST%%:*}"
  SUFFIX="${REST##*:}"

  echo "==> Building $TRIPLE (vsce target: $VSCE_TARGET)"
  cargo build --release --target "$TRIPLE" --manifest-path "$ROOT/Cargo.toml"

  cp "$ROOT/target/$TRIPLE/release/$ENGINE_NAME$SUFFIX" \
     "$EXT_DIR/server/$ENGINE_NAME$SUFFIX"

  (cd "$EXT_DIR" && npm run package -- --target "$VSCE_TARGET" \
     -o "$OUT_DIR/git-bamboo-$VERSION-$VSCE_TARGET.vsix")

  # Remove the binary so a later iteration can never package a stale
  # platform build.
  rm -f "$EXT_DIR/server/$ENGINE_NAME$SUFFIX"
done

echo "==> Done. VSIX packages are in $OUT_DIR/"
