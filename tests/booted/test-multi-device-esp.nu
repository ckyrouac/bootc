# Test multi-device ESP detection for to-existing-root
#
# Test that bootc install to-existing-root can find and use ESP partitions
# when the root filesystem spans multiple backing devices (e.g., LVM across disks).
#
# Five scenarios are tested:
#   1. Single ESP: Only one of two backing devices has an ESP partition
#   2. Dual ESP: Both backing devices have ESP partitions
#   3. Three devices, partial ESP: Three disks, ESP on disk1+disk3 only
#   4. Single device (no LVM): ESP + root partition on a single disk
#   5. No ESP anywhere: Two disks with no ESP; install should fail gracefully
#
# This validates the backport of multi-device parent support from v1.15.x
# that enables Intel VROC RAID and multipath installations.

use std assert
use tap.nu

const target_image = "localhost/bootc"

# Nushell buffers stdout and can lose it if the script exits abruptly on an
# uncaught error, which makes `print`-based diagnostics unreliable when
# captured over the tmt/ssh execution path. Mirror everything to a plain
# file as well, since file writes are flushed synchronously and survive
# regardless of how the process exits.
const DEBUG_LOG = "/var/tmp/bootc-esp-test.log"

def log [msg: string] {
    print $msg
    $"($msg)\n" | save --append $DEBUG_LOG
}

# ESP partition type GUID
const ESP_TYPE = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
# Linux LVM partition type GUID
const LVM_TYPE = "E6D6D379-F507-44C2-A23C-238F2A3DF928"
# Linux root (x86-64) partition type GUID
const ROOT_TYPE = "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709"

# Cleanup function for LVM and loop devices
def cleanup [vg_name: string, loops: list<string>, mountpoint: string] {
    # Unmount if mounted
    do { umount $mountpoint } | complete | ignore
    do { rmdir $mountpoint } | complete | ignore

    # Deactivate and remove LVM
    do { lvchange -an $"($vg_name)/test_lv" } | complete | ignore
    do { lvremove -f $"($vg_name)/test_lv" } | complete | ignore
    do { vgchange -an $vg_name } | complete | ignore
    do { vgremove -f $vg_name } | complete | ignore

    # Remove PVs from partitions and detach loop devices
    for loop in $loops {
        if ($loop | path exists) {
            for i in [1, 2, 3] {
                let part = $"($loop)p($i)"
                if ($part | path exists) {
                    do { pvremove -f $part } | complete | ignore
                    do { wipefs -a $part } | complete | ignore
                }
            }
            do { udevadm settle } | complete | ignore
            do { partx -d $loop } | complete | ignore
            do { losetup -d $loop } | complete | ignore
        }
    }

    rm -f /etc/lvm/devices/system.devices
}

# Create a disk with GPT, optional ESP, and LVM partition
# Returns the loop device path
def setup_disk_with_partitions [
    disk_path: string,
    with_esp: bool,
    disk_size: string = "5G"
] {
    # Create disk image
    truncate -s $disk_size $disk_path

    # Setup loop device
    let loop = (losetup -f --show $disk_path | str trim)

    # Create partition table
    if $with_esp {
        # GPT with ESP (512MB) + LVM partition
        $"label: gpt\nsize=512M, type=($ESP_TYPE)\ntype=($LVM_TYPE)\n" | sfdisk $loop

        # Remove stale partition entries then add new ones
        do { partx -d $loop } | complete | ignore
        partx -av $loop
        udevadm settle

        # Format ESP
        mkfs.vfat -F 32 $"($loop)p1"
    } else {
        # GPT with only LVM partition (full disk)
        $"label: gpt\ntype=($LVM_TYPE)\n" | sfdisk $loop

        do { partx -d $loop } | complete | ignore
        partx -av $loop
        udevadm settle
    }

    $loop
}

# Create a disk with GPT, ESP, and a root partition (no LVM)
# Returns the loop device path
def setup_disk_with_root [
    disk_path: string,
    disk_size: string = "5G"
] {
    truncate -s $disk_size $disk_path
    let loop = (losetup -f --show $disk_path | str trim)

    # GPT with ESP (512MB) + root partition
    $"label: gpt\nsize=512M, type=($ESP_TYPE)\ntype=($ROOT_TYPE)\n" | sfdisk $loop
    do { partx -d $loop } | complete | ignore
    partx -av $loop
    udevadm settle

    mkfs.vfat -F 32 $"($loop)p1"
    mkfs.ext4 -q $"($loop)p2"

    $loop
}

# Simple cleanup for non-LVM scenarios (single loop device, no VG)
def cleanup_simple [loop: string, mountpoint: string] {
    do { umount $mountpoint } | complete | ignore
    do { rmdir $mountpoint } | complete | ignore

    if ($loop | path exists) {
        do { losetup -d $loop } | complete | ignore
    }
}

# Validate that an ESP partition has bootloader files installed
def validate_esp [esp_partition: string] {
    let esp_mount = "/var/mnt/esp_check"
    mkdir $esp_mount
    mount $esp_partition $esp_mount

    # Check for EFI directory with bootloader files
    let efi_dir = $"($esp_mount)/EFI"
    if not ($efi_dir | path exists) {
        umount $esp_mount
        rmdir $esp_mount
        error make {msg: $"ESP validation failed: EFI directory not found on ($esp_partition)"}
    }

    # Verify there's actual content in EFI (not just empty)
    let efi_contents = (ls $efi_dir | length)
    umount $esp_mount
    rmdir $esp_mount

    if $efi_contents == 0 {
        error make {msg: $"ESP validation failed: EFI directory is empty on ($esp_partition)"}
    }
}

# Run bootc install to-existing-root from within the container image under test.
#
# Captures full stdout/stderr via `complete` and logs them (see `log`) so the
# install output is diagnosable even when nushell's own stdout buffering
# would otherwise lose `print` output on an abrupt/erroring exit.
#
# When `expect_failure` is false (the default), a non-zero exit raises an
# error immediately with the captured stderr included in the message. When
# `expect_failure` is true, the completion record is returned as-is so the
# caller can make its own assertions (see test_no_esp_failure).
def run_install [mountpoint: string, expect_failure: bool = false] {
    let result = (do {
        (podman run
            --rm
            --privileged
            --pull never
            -v $"($mountpoint):/target"
            -v /dev:/dev
            -v /run/udev:/run/udev:ro
            -v /usr/share/empty:/usr/lib/bootc/bound-images.d
            --pid=host
            --security-opt label=type:unconfined_t
            --env BOOTC_BOOTLOADER_DEBUG=1
            $target_image
            # bootc install defaults to fetching the *target* image (i.e. the
            # image future upgrades will pull) via the `registry` transport,
            # which would try to pull "localhost/bootc" from a registry at
            # localhost:443 and fail since there is none. The image was loaded
            # into local podman storage via `bootc image copy-to-storage`, so
            # tell it to use that instead.
            bootc install to-existing-root
                --disable-selinux
                --acknowledge-destructive
                --target-no-signature-verification
                --target-transport containers-storage
                /target)
    } | complete)

    log $"run_install exit_code: ($result.exit_code)"
    log $"run_install stdout:\n($result.stdout)"
    log $"run_install stderr:\n($result.stderr)"

    if $result.exit_code != 0 and not $expect_failure {
        error make {msg: $"bootc install to-existing-root failed with exit code ($result.exit_code): ($result.stderr)"}
    }

    $result
}

# Test scenario 1: Single ESP on first device
def test_single_esp [] {
    log "Starting single ESP test"

    let vg_name = "test_single_esp_vg"
    let mountpoint = "/var/mnt/test_single_esp"
    let disk1 = "/var/tmp/disk1_single.img"
    let disk2 = "/var/tmp/disk2_single.img"

    # DISK1: ESP + LVM partition
    # DISK2: Full LVM partition (no ESP)
    let loop1 = (setup_disk_with_partitions $disk1 true)
    let loop2 = (setup_disk_with_partitions $disk2 false)

    try {
        pvcreate $"($loop1)p2" $"($loop2)p1"
        vgcreate $vg_name $"($loop1)p2" $"($loop2)p1"
        lvcreate -l "100%FREE" -n test_lv $vg_name

        let lv_path = $"/dev/($vg_name)/test_lv"

        mkfs.ext4 -q $lv_path
        mkdir $mountpoint
        mount $lv_path $mountpoint
        mkdir $"($mountpoint)/boot"

        lsblk --pairs --paths --inverse --output NAME,TYPE $lv_path

        run_install $mountpoint

        validate_esp $"($loop1)p1"
    } catch {|e|
        cleanup $vg_name [$loop1, $loop2] $mountpoint
        rm -f $disk1 $disk2
        error make {msg: $"Single ESP test failed: ($e)"}
    }

    cleanup $vg_name [$loop1, $loop2] $mountpoint
    rm -f $disk1 $disk2

    log "Single ESP test completed successfully"
}

# Test scenario 2: ESP on both devices
def test_dual_esp [] {
    log "Starting dual ESP test"

    let vg_name = "test_dual_esp_vg"
    let mountpoint = "/var/mnt/test_dual_esp"
    let disk1 = "/var/tmp/disk1_dual.img"
    let disk2 = "/var/tmp/disk2_dual.img"

    # DISK1: ESP + LVM partition
    # DISK2: ESP + LVM partition
    let loop1 = (setup_disk_with_partitions $disk1 true)
    let loop2 = (setup_disk_with_partitions $disk2 true)

    try {
        pvcreate $"($loop1)p2" $"($loop2)p2"
        vgcreate $vg_name $"($loop1)p2" $"($loop2)p2"
        lvcreate -l "100%FREE" -n test_lv $vg_name

        let lv_path = $"/dev/($vg_name)/test_lv"

        mkfs.ext4 -q $lv_path
        mkdir $mountpoint
        mount $lv_path $mountpoint
        mkdir $"($mountpoint)/boot"

        lsblk --pairs --paths --inverse --output NAME,TYPE $lv_path

        run_install $mountpoint

        validate_esp $"($loop1)p1"
        validate_esp $"($loop2)p1"
    } catch {|e|
        cleanup $vg_name [$loop1, $loop2] $mountpoint
        rm -f $disk1 $disk2
        error make {msg: $"Dual ESP test failed: ($e)"}
    }

    cleanup $vg_name [$loop1, $loop2] $mountpoint
    rm -f $disk1 $disk2

    log "Dual ESP test completed successfully"
}

# Test scenario 3: Three devices, ESP on disk1 and disk3 only
def test_three_devices_partial_esp [] {
    log "Starting three devices partial ESP test"

    let vg_name = "test_three_dev_vg"
    let mountpoint = "/var/mnt/test_three_dev"
    let disk1 = "/var/tmp/disk1_three.img"
    let disk2 = "/var/tmp/disk2_three.img"
    let disk3 = "/var/tmp/disk3_three.img"

    # DISK1: ESP + LVM partition
    # DISK2: Full LVM partition (no ESP)
    # DISK3: ESP + LVM partition
    let loop1 = (setup_disk_with_partitions $disk1 true)
    let loop2 = (setup_disk_with_partitions $disk2 false)
    let loop3 = (setup_disk_with_partitions $disk3 true)

    try {
        pvcreate $"($loop1)p2" $"($loop2)p1" $"($loop3)p2"
        vgcreate $vg_name $"($loop1)p2" $"($loop2)p1" $"($loop3)p2"
        lvcreate -l "100%FREE" -n test_lv $vg_name

        let lv_path = $"/dev/($vg_name)/test_lv"

        mkfs.ext4 -q $lv_path
        mkdir $mountpoint
        mount $lv_path $mountpoint
        mkdir $"($mountpoint)/boot"

        lsblk --pairs --paths --inverse --output NAME,TYPE $lv_path

        run_install $mountpoint

        validate_esp $"($loop1)p1"
        validate_esp $"($loop3)p1"
    } catch {|e|
        cleanup $vg_name [$loop1, $loop2, $loop3] $mountpoint
        rm -f $disk1 $disk2 $disk3
        error make {msg: $"Three devices partial ESP test failed: ($e)"}
    }

    cleanup $vg_name [$loop1, $loop2, $loop3] $mountpoint
    rm -f $disk1 $disk2 $disk3

    log "Three devices partial ESP test completed successfully"
}

# Test scenario 4: Single device with ESP + root partition (no LVM)
def test_single_device_no_lvm [] {
    log "Starting single device no LVM test"

    let mountpoint = "/var/mnt/test_no_lvm"
    let disk1 = "/var/tmp/disk1_nolvm.img"

    let loop1 = (setup_disk_with_root $disk1 "10G")

    try {
        mkdir $mountpoint
        mount $"($loop1)p2" $mountpoint
        mkdir $"($mountpoint)/boot"

        lsblk --pairs --paths --inverse --output NAME,TYPE $"($loop1)p2"

        run_install $mountpoint

        validate_esp $"($loop1)p1"
    } catch {|e|
        cleanup_simple $loop1 $mountpoint
        rm -f $disk1
        error make {msg: $"Single device no LVM test failed: ($e)"}
    }

    cleanup_simple $loop1 $mountpoint
    rm -f $disk1

    log "Single device no LVM test completed successfully"
}

# Test scenario 5: No ESP on any device (install should fail gracefully)
def test_no_esp_failure [] {
    log "Starting no ESP failure test"

    let vg_name = "test_no_esp_vg"
    let mountpoint = "/var/mnt/test_no_esp"
    let disk1 = "/var/tmp/disk1_noesp.img"
    let disk2 = "/var/tmp/disk2_noesp.img"

    # Neither disk has an ESP
    let loop1 = (setup_disk_with_partitions $disk1 false)
    let loop2 = (setup_disk_with_partitions $disk2 false)

    try {
        pvcreate $"($loop1)p1" $"($loop2)p1"
        vgcreate $vg_name $"($loop1)p1" $"($loop2)p1"
        lvcreate -l "100%FREE" -n test_lv $vg_name

        let lv_path = $"/dev/($vg_name)/test_lv"

        mkfs.ext4 -q $lv_path
        mkdir $mountpoint
        mount $lv_path $mountpoint
        mkdir $"($mountpoint)/boot"

        lsblk --pairs --paths --inverse --output NAME,TYPE $lv_path

        # Run install and expect it to fail
        let result = (run_install $mountpoint true)

        assert ($result.exit_code != 0) "Expected install to fail with no ESP partitions"
        # Verify the failure is ESP-related
        let combined = $"($result.stdout)\n($result.stderr)"
        assert ($combined | str contains "ESP") $"Expected ESP-related error message, got: ($combined | str substring 0..200)"
        log $"Install failed as expected with exit code ($result.exit_code)"
    } catch {|e|
        cleanup $vg_name [$loop1, $loop2] $mountpoint
        rm -f $disk1 $disk2
        error make {msg: $"No ESP failure test failed: ($e)"}
    }

    cleanup $vg_name [$loop1, $loop2] $mountpoint
    rm -f $disk1 $disk2

    log "No ESP failure test completed successfully"
}

def main [] {
    # Start with a clean debug log for this run (see `log`/`DEBUG_LOG`).
    rm -f $DEBUG_LOG

    tap begin "multi-device ESP detection tests"

    # This test requires a UEFI-booted host because it creates ESP partitions
    # and expects bootupd to install a UEFI bootloader. On BIOS systems,
    # bootupd would try to install GRUB for i386-pc which needs a BIOS Boot
    # Partition instead of an ESP.
    if not ("/sys/firmware/efi" | path exists) {
        log "SKIP: multi-device ESP test requires UEFI boot"
        tap ok
        return
    }

    log "UEFI detected, starting tests"
    # Copy the booted image into podman container storage so 'podman run localhost/bootc' works.
    # If the booted image transport is 'registry' pointing to localhost, the copy will read
    # from the ostree object store (not the network) and export to containers-storage.
    # Use --pull never in podman run (see run_install) to prevent network pull attempts.
    log "=== Running bootc image copy-to-storage ==="
    let copy_result = (do { bootc image copy-to-storage } | complete)
    log $"bootc image copy-to-storage exit code: ($copy_result.exit_code)"
    log $"stdout: ($copy_result.stdout | str substring 0..200)"
    log $"stderr: ($copy_result.stderr | str substring 0..200)"
    if $copy_result.exit_code != 0 {
        error make {msg: $"bootc image copy-to-storage failed: ($copy_result.stderr)"}
    }
    log "=== bootc image copy-to-storage done ==="
    # Verify the image is now in podman storage
    let img_check = (do { podman image inspect $target_image } | complete)
    log $"Image in podman storage: ($img_check.exit_code == 0)"

    log "=== Starting test_single_esp ==="
    test_single_esp
    log "=== PASSED: test_single_esp ==="

    log "=== Starting test_dual_esp ==="
    test_dual_esp
    log "=== PASSED: test_dual_esp ==="

    log "=== Starting test_three_devices_partial_esp ==="
    test_three_devices_partial_esp
    log "=== PASSED: test_three_devices_partial_esp ==="

    log "=== Starting test_single_device_no_lvm ==="
    test_single_device_no_lvm
    log "=== PASSED: test_single_device_no_lvm ==="

    log "=== Starting test_no_esp_failure ==="
    test_no_esp_failure
    log "=== PASSED: test_no_esp_failure ==="

    log "=== ALL TESTS PASSED ==="

    tap ok
}
