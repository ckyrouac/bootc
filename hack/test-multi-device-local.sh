#!/bin/bash
# Test multi-device ESP detection locally using loopback disks with LVM.
#
# This is a local equivalent of tmt/tests/booted/test-multi-device-esp.nu
# that runs `bootc install to-filesystem` on LVM backed by two loopback disks.
#
# Two scenarios are tested:
# 1. Single ESP: Only one backing device has an ESP partition
# 2. Dual ESP: Both backing devices have ESP partitions
#
# Prerequisites:
#   - A locally built bootc container image (run `just build` first)
#   - A bootupd source tree at BOOTUPD_DIR (default: ~/projects/bootupd)
#   - Must be run as root
#   - Required packages: lvm2, dosfstools, e2fsprogs, util-linux, podman

set -xeuo pipefail

BOOTUPD_DIR="${BOOTUPD_DIR:-$HOME/projects/bootupd}"
BOOTC_DIR="$(cd "$(dirname "$0")/.." && pwd)"

# ESP partition type GUID
ESP_TYPE="C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
# Linux LVM partition type GUID
LVM_TYPE="E6D6D379-F507-44C2-A23C-238F2A3DF928"

BASE_IMAGE="${TARGET_IMAGE:-localhost/bootc}"
TARGET_IMAGE="localhost/bootc-local-bootupd"
DISK_SIZE="${DISK_SIZE:-5G}"

# --------------------------------------------------------------------------
# Build local bootupd and create a derived container image with it
# --------------------------------------------------------------------------
echo "==> Building bootupd from $BOOTUPD_DIR..."
(cd "$BOOTUPD_DIR" && cargo build --release)
echo "==> Local bootupd version:"
"$BOOTUPD_DIR/target/release/bootupd" --version

echo "==> Building derived container image with local bootupd..."
podman build -t "$TARGET_IMAGE" --build-arg="base=$BASE_IMAGE" \
    -f - "$BOOTUPD_DIR/target/release" <<'EOF'
ARG base=localhost/bootc
FROM $base
COPY bootupd /usr/libexec/bootupd
COPY bootupd /usr/bin/bootupctl
EOF

# Track loop devices for cleanup
LOOP1=""
LOOP2=""

cleanup() {
    set +e
    echo "==> Cleaning up..."

    # Unmount if mounted
    umount "$MOUNTPOINT" 2>/dev/null
    rmdir "$MOUNTPOINT" 2>/dev/null

    # Deactivate and remove LVM
    lvchange -an "${VG_NAME}/test_lv" 2>/dev/null
    lvremove -f "${VG_NAME}/test_lv" 2>/dev/null
    vgchange -an "$VG_NAME" 2>/dev/null
    vgremove -f "$VG_NAME" 2>/dev/null

    # Remove PVs and detach loop devices
    if [ -n "$LOOP1" ] && [ -e "$LOOP1" ]; then
        pvremove -f "${LOOP1}p2" 2>/dev/null || pvremove -f "${LOOP1}p1" 2>/dev/null
        losetup -d "$LOOP1" 2>/dev/null
    fi
    if [ -n "$LOOP2" ] && [ -e "$LOOP2" ]; then
        pvremove -f "${LOOP2}p2" 2>/dev/null || pvremove -f "${LOOP2}p1" 2>/dev/null
        losetup -d "$LOOP2" 2>/dev/null
    fi

    # Remove disk images
    rm -f "$DISK1" "$DISK2"
}

# Create a disk with GPT, optional ESP, and LVM partition.
# Sets REPLY to the loop device path.
setup_disk_with_partitions() {
    local disk_path="$1"
    local with_esp="$2"

    truncate -s "$DISK_SIZE" "$disk_path"
    local loop
    loop=$(losetup -f --show "$disk_path")

    if [ "$with_esp" = "true" ]; then
        # GPT with ESP (512MB) + LVM partition
        printf 'label: gpt\nsize=512M, type=%s\ntype=%s\n' "$ESP_TYPE" "$LVM_TYPE" | sfdisk "$loop"
    else
        # GPT with only LVM partition (full disk)
        printf 'label: gpt\ntype=%s\n' "$LVM_TYPE" | sfdisk "$loop"
    fi

    # Reload partition table
    partx -u "$loop"
    sleep 1

    if [ "$with_esp" = "true" ]; then
        mkfs.vfat -F 32 "${loop}p1"
    fi

    REPLY="$loop"
}

# Validate that an ESP partition has bootloader files installed
validate_esp() {
    local esp_partition="$1"
    local esp_mount="/var/mnt/esp_check"

    mkdir -p "$esp_mount"
    mount "$esp_partition" "$esp_mount"

    local efi_dir="$esp_mount/EFI"
    if [ ! -d "$efi_dir" ]; then
        umount "$esp_mount"
        rmdir "$esp_mount"
        echo "ERROR: ESP validation failed: EFI directory not found on $esp_partition"
        return 1
    fi

    local efi_count
    efi_count=$(ls "$efi_dir" | wc -l)
    umount "$esp_mount"
    rmdir "$esp_mount"

    if [ "$efi_count" -eq 0 ]; then
        echo "ERROR: ESP validation failed: EFI directory is empty on $esp_partition"
        return 1
    fi

    echo "ESP validation passed for $esp_partition"
}

# Run bootc install to-filesystem from the container image
run_install() {
    local mountpoint="$1"

    podman run \
        --rm --privileged \
        -v "${mountpoint}:/target" \
        -v /dev:/dev \
        -v /usr/share/empty:/usr/lib/bootc/bound-images.d \
        --pid=host \
        --security-opt label=type:unconfined_t \
        --env BOOTC_BOOTLOADER_DEBUG=1 \
        "$TARGET_IMAGE" \
        bootc install to-filesystem \
            --disable-selinux \
            --target-no-signature-verification \
            /target
}

# --------------------------------------------------------------------------
# Test scenario 1: Single ESP on first device
# --------------------------------------------------------------------------
test_single_esp() {
    echo "==> Starting single ESP test"

    VG_NAME="test_single_esp_vg"
    MOUNTPOINT="/var/mnt/test_single_esp"
    DISK1="/var/tmp/disk1_single.img"
    DISK2="/var/tmp/disk2_single.img"

    # DISK1: ESP + LVM partition
    setup_disk_with_partitions "$DISK1" true
    LOOP1="$REPLY"

    # DISK2: Full LVM partition (no ESP)
    setup_disk_with_partitions "$DISK2" false
    LOOP2="$REPLY"

    trap cleanup EXIT

    # Create LVM spanning both devices
    # Partition 2 from disk1 (after ESP) and partition 1 from disk2 (full disk)
    pvcreate "${LOOP1}p2" "${LOOP2}p1"
    vgcreate "$VG_NAME" "${LOOP1}p2" "${LOOP2}p1"
    lvcreate -l "100%FREE" -n test_lv "$VG_NAME"

    local lv_path="/dev/${VG_NAME}/test_lv"

    mkfs.ext4 -q "$lv_path"
    mkdir -p "$MOUNTPOINT"
    mount "$lv_path" "$MOUNTPOINT"

    # Create boot directory
    mkdir -p "${MOUNTPOINT}/boot"

    # Show block device hierarchy
    lsblk --pairs --paths --inverse --output NAME,TYPE "$lv_path"

    run_install "$MOUNTPOINT"

    # Validate ESP was installed correctly
    validate_esp "${LOOP1}p1"

    # Cleanup before next test
    cleanup
    LOOP1=""
    LOOP2=""
    trap - EXIT

    echo "==> Single ESP test completed successfully"
}

# --------------------------------------------------------------------------
# Test scenario 2: ESP on both devices
# --------------------------------------------------------------------------
test_dual_esp() {
    echo "==> Starting dual ESP test"

    VG_NAME="test_dual_esp_vg"
    MOUNTPOINT="/var/mnt/test_dual_esp"
    DISK1="/var/tmp/disk1_dual.img"
    DISK2="/var/tmp/disk2_dual.img"

    # DISK1: ESP + LVM partition
    setup_disk_with_partitions "$DISK1" true
    LOOP1="$REPLY"

    # DISK2: ESP + LVM partition
    setup_disk_with_partitions "$DISK2" true
    LOOP2="$REPLY"

    trap cleanup EXIT

    # Create LVM spanning both devices
    # Use partition 2 from both disks (after ESP)
    pvcreate "${LOOP1}p2" "${LOOP2}p2"
    vgcreate "$VG_NAME" "${LOOP1}p2" "${LOOP2}p2"
    lvcreate -l "100%FREE" -n test_lv "$VG_NAME"

    local lv_path="/dev/${VG_NAME}/test_lv"

    mkfs.ext4 -q "$lv_path"
    mkdir -p "$MOUNTPOINT"
    mount "$lv_path" "$MOUNTPOINT"

    # Create boot directory
    mkdir -p "${MOUNTPOINT}/boot"

    # Show block device hierarchy
    lsblk --pairs --paths --inverse --output NAME,TYPE "$lv_path"

    run_install "$MOUNTPOINT"

    # Validate both ESPs were installed correctly
    validate_esp "${LOOP1}p1"
    validate_esp "${LOOP2}p1"

    cleanup
    LOOP1=""
    LOOP2=""
    trap - EXIT

    echo "==> Dual ESP test completed successfully"
}

# --------------------------------------------------------------------------
# Main
# --------------------------------------------------------------------------
echo "==> Multi-device ESP local test"
echo "==> Base image: $BASE_IMAGE"
echo "==> Test image: $TARGET_IMAGE (with local bootupd)"

test_single_esp
# test_dual_esp

echo "==> All multi-device ESP tests passed"
