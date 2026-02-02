#!/bin/bash
# Test multi-device ESP with local bootupd binary
set -euo pipefail

BOOTUPD_DIR="${BOOTUPD_DIR:-$HOME/projects/bootupd}"
BOOTC_DIR="$(cd "$(dirname "$0")/.." && pwd)"

echo "==> Building bootupd with LOCAL_BUILD marker..."
cd "$BOOTUPD_DIR"
LOCAL_BUILD="$(date +%Y%m%d-%H%M%S)" cargo build

echo "==> Copying bootupd to test tree..."
cp target/debug/bootupd "$BOOTC_DIR/tmt/tests/booted/local-bootupd"

echo "==> Verifying version..."
ln -sf "$BOOTC_DIR/tmt/tests/booted/local-bootupd" /tmp/bootupctl
/tmp/bootupctl --version

echo "==> Adding local-bootupd to git (required for TMT tree)..."
cd "$BOOTC_DIR"
# Force-add the file since it's in .gitignore - TMT needs it in the git tree
git add -f tmt/tests/booted/local-bootupd

# Cleanup function to remove the staged file
cleanup() {
    echo "==> Cleaning up staged local-bootupd..."
    git reset HEAD -- tmt/tests/booted/local-bootupd 2>/dev/null || true
}
trap cleanup EXIT

echo "==> Running TMT test..."
# Pass USE_LOCAL_BOOTUPD via --env flag since just/xtask doesn't inherit shell env
just test-tmt --env=USE_LOCAL_BOOTUPD=1 multi-device-esp
