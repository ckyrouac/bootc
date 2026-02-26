#!/bin/bash
# Test multi-device ESP with local bootupd binary
#
# This script builds a local bootupd and injects it into the container image
# so that bootc install (during VM creation) uses the correct bootupd version.
set -euo pipefail

BOOTUPD_DIR="${BOOTUPD_DIR:-$HOME/projects/bootupd}"
BOOTC_DIR="$(cd "$(dirname "$0")/.." && pwd)"

echo "==> Building bootupd with LOCAL_BUILD marker..."
cd "$BOOTUPD_DIR"
LOCAL_BUILD="$(date +%Y%m%d-%H%M%S)" cargo build --release

echo "==> Copying bootupd to bootc tree for container image injection..."
# Copy to contrib/packaging where it will be picked up during container build
mkdir -p "$BOOTC_DIR/contrib/packaging/local-bootupd"
cp target/release/bootupd "$BOOTC_DIR/contrib/packaging/local-bootupd/"

echo "==> Verifying version..."
"$BOOTC_DIR/contrib/packaging/local-bootupd/bootupd" --version

echo "==> Also copying to test tree for test-phase injection..."
cp target/release/bootupd "$BOOTC_DIR/tmt/tests/booted/local-bootupd"

echo "==> Adding local-bootupd to git (required for TMT tree)..."
cd "$BOOTC_DIR"
# Force-add the file since it's in .gitignore - TMT needs it in the git tree
git add -f tmt/tests/booted/local-bootupd

# Cleanup function to remove the staged file and local-bootupd dir
cleanup() {
    echo "==> Cleaning up staged local-bootupd..."
    git reset HEAD -- tmt/tests/booted/local-bootupd 2>/dev/null || true
    rm -rf "$BOOTC_DIR/contrib/packaging/local-bootupd"
}
trap cleanup EXIT

echo "==> Running TMT test (this will rebuild the container image with local bootupd)..."
# Pass USE_LOCAL_BOOTUPD via --env flag since just/xtask doesn't inherit shell env
#
# Use composefs-sealeduki-sdboot variant to build a composefs image.
# This enables testing of:
# - systemd-boot on LVM (requires composefs for direct installation)
# - composefs+grub on LVM
#
# The multi-device-esp test covers all LVM scenarios:
# - Single ESP with grub (to-existing-root)
# - Dual ESP with grub (to-existing-root)
# - systemd-boot on LVM (to-filesystem, composefs only)
# - zIPL on LVM (to-filesystem, s390x only)
# - grub on LVM (to-filesystem)
just variant=composefs-sealeduki-sdboot test-tmt --env=USE_LOCAL_BOOTUPD=1 multi-device-esp
