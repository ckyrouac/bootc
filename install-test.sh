#!/bin/bash

#deps
#dnf install -y bootupd

mkdir -p /var/mnt
MOUNT=/var/mnt

set -x
umount "$MOUNT"

set -e

rm -rf disk.img
truncate -s 10G disk.img
DEVICE=$(losetup -f --show disk.img)
parted "$DEVICE" --script mklabel gpt
parted "$DEVICE" --script mkpart primary ext4 0% 100%
partprobe "${DEVICE}"
mkfs.ext4 "${DEVICE}p1"
mount "${DEVICE}p1" "$MOUNT"
mkdir "${MOUNT}/boot"

export RUST_LOG=debug
export BOOTC_BOOTLOADER_DEBUG=-vvvv
./target/release/bootc install to-filesystem --source-imgref docker://quay.io/centos-bootc/centos-bootc:stream9 "$MOUNT"
# ./target/release/bootc install to-filesystem --source-imgref containers-storage://quay.io/centos-bootc/centos-bootc:stream9 "$MOUNT"
# podman run --pid=host --network=host --privileged --security-opt label=type:unconfined_t -v /dev:/dev -v /var/mnt:/foo quay.io/centos-bootc/centos-bootc:stream9 bootc install to-filesystem /foo
