# `bootc install from-existing-root` Integration Test — Status

**Last updated:** 2026-06-11

## Summary

The integration test at `tests/from-existing-root/run` is **partially working** but
blocked by a kernel-level crash in the Fedora 44 VM that occurs during large writes
to btrfs.

---

## What the Test Does

1. Starts a fresh Fedora 44 VM (package-mode, btrfs root filesystem)
2. Installs buildah, ostree, skopeo, bootupctl, and the `bootc` binary into the VM
3. Runs `bootc install from-existing-root` which:
   - Creates a buildah container from scratch
   - Tars the running root filesystem into the container (the snapshot)
   - Commits the container as a local OCI image
   - Runs `podman run ... bootc install to-existing-root` to deploy it
4. Reboots the VM and verifies `bootc status` reports a booted image

---

## Current Test Results

| Phase | Status | Notes |
|-------|--------|-------|
| VM setup (Fedora 44, package mode) | ✅ Works | Cloud-init boots cleanly |
| Install tools into VM | ✅ Works | buildah, ostree, skopeo, bootupctl all install |
| `bootc` binary functional in VM | ✅ Works | `bootc --version` reports correctly |
| `from-existing-root` subcommand present | ✅ Works | |
| `buildah from scratch` | ✅ Works | |
| `tar` pass 1 (snapshot root to tmpfs) | ✅ Works | After fix to write to `/tmp` instead of `/var/tmp` |
| `tar` pass 2 (append btrfs subvolumes) | ✅ Works | After fix to detect subvolumes by block device |
| `buildah add` (unpack tar into container) | ❌ **CRASHES VM** | Large writes to btrfs crash the guest |
| `buildah commit` | Not reached | |
| `podman run bootc install to-existing-root` | Not reached | |
| Reboot + verify `bootc status` | Not reached | |

---

## Root Cause of Blocker

**Kernel bug:** The Fedora 44 VM runs kernel `6.19.10-300.fc44.x86_64` with a btrfs
root filesystem. Any large sequential write to btrfs (~1GB+) causes a **hard guest
crash** (QEMU reports `reason=crashed`, no kernel panic message, no dmesg output
before crash).

### Confirmed crash triggers:
- `buildah add container big.tar /` — extracts 1.1GB tar into VFS storage on btrfs
- `dd if=/dev/zero of=/var/tmp/big.img bs=1M count=3000` — raw 3GB write to btrfs

### Confirmed non-crash operations:
- Writing the same data to `/tmp` (tmpfs) — no crash
- Reading from btrfs (tar piped to `wc -c`) — no crash
- `buildah add` with a small (~220MB) tar — no crash
- Disabling zstd compression (`mount -o remount,compress=no`) — still crashes

### Closest known bug:
A regression reported on LKML in January 2026 describes VMs crashing under heavy
btrfs I/O with zstd compression on kernels 6.18+, while kernel 6.12 LTS is stable.
Thread: https://lkml.org/lkml/2026/1/11/137

---

## Code Fixes Made (Already Committed/Applied)

### 1. Tar output location: `/var/tmp` → `/tmp`
**File:** `crates/lib/src/install/from_existing.rs`  
**Function:** `choose_tar_dir()` + `snapshot_filesystem()`

Writing the intermediate tar archive to `/tmp` (tmpfs) instead of `/var/tmp` (btrfs)
avoids one crash trigger. The `tar` passes themselves now complete without crashing.

### 2. btrfs subvolume detection: `st_dev` → block device string
**File:** `crates/lib/src/install/from_existing.rs`  
**Function:** `snapshot_filesystem()`

**Bug:** The original code used `st_dev` (from `stat()`) to identify "same physical
device" mounts for the multi-pass tar. On btrfs, each subvolume reports a *different*
`st_dev` even though all subvolumes share one physical disk. This caused `/var`,
`/home`, and `/boot` to be silently excluded from the snapshot, producing a broken
image with empty `/var`.

**Fix:** Compare the block device string from `/proc/mounts` (e.g. `/dev/vda3`) for
the root mount against all other mounts. All btrfs subvolumes share the same device
string.

### 3. Tar exclude patterns: leading `/` stripped
**File:** `crates/lib/src/install/from_existing.rs`  
**Arrays:** `EXCLUDED_PATHS` processing

GNU tar strips the leading `/` from archive member names, so `--exclude=/proc` never
matched anything. Fixed to pass `--exclude=proc`.

### 4. Circular tar append prevention
**File:** `crates/lib/src/install/from_existing.rs`

When the tar archive lives inside a btrfs subvolume that is also being archived in
pass 2, the archive file itself would be included in the append, causing unbounded
growth. Fixed by explicitly excluding the tar file path from each pass 2 invocation.

---

## Ideas for Future Resolution

### Option A: Switch to ext4 base image (recommended)
Use a Fedora 44 VM image formatted with ext4 instead of btrfs. The cloud image uses
btrfs by default; a custom qcow2 with ext4 would avoid the kernel bug entirely.

Steps:
1. Create a fresh qcow2 with ext4: `virt-builder fedora-44 --format qcow2 --root-password ...`
2. Or convert the existing image: boot a live ISO and reformat with ext4
3. Or use `quay.io/containerdisks/fedora:44` (already used) but repartition during
   VM setup using cloud-init's `runcmd` to reformat as ext4

### Option B: Use Fedora 41/42 (stable kernel)
The LKML reporter confirmed kernel 6.12 LTS does not exhibit this crash. Fedora 41
ships kernel ~6.11/6.12. The test could be parameterized to use `quay.io/containerdisks/fedora:41`.

### Option C: Mount tmpfs over `/var/lib/containers`
Before running `buildah add`, bind-mount a large tmpfs over `/var/lib/containers`
so VFS storage writes go to RAM instead of btrfs. Requires enough RAM (4GB VM, ~2GB
needed for extracted content — tight but potentially workable with 4GB RAM + swap).

```bash
sudo mount -t tmpfs -o size=3G tmpfs /var/lib/containers
```

Requires expanding VM RAM to 6–8GB to be safe.

### Option D: Use `ostree` directly instead of buildah/podman
Instead of `buildah add` (which writes thousands of small files to btrfs), use
`ostree commit` to create the OCI image directly. `ostree` uses hardlinks and a
content-addressed store; the write pattern is very different and may not trigger
the crash.

This would require significant refactoring of `from_existing.rs`.

### Option E: Wait for kernel fix
Monitor the LKML thread (https://lkml.org/lkml/2026/1/11/137) for a fix. If a
patch is merged into the 6.19.x stable series or into Fedora 44's kernel updates,
upgrading the kernel in the VM image would resolve the crash without code changes.

Check: `dnf check-update kernel` inside the VM to see if a newer kernel is available.

### Option F: Use `buildah add` with a pipe rather than a file
Instead of writing the tar to a tmpfs file and then calling `buildah add file`,
use a named pipe (FIFO) so tar streams directly into buildah without any large file
on disk. This avoids both the btrfs write *and* the tmpfs size limitation.

The challenge: `buildah add` does not accept stdin (`-`), but accepts a named pipe
path. The pipe reader (buildah) and writer (tar) need to run concurrently, requiring
a thread or subprocess. Tested briefly; the approach works for small tars but
deadlocked for large ones (likely buildah trying to seek in the stream).

---

## Test Infrastructure Notes

- **QEMU URI:** `qemu+unix:///session?socket=/tmp/bootc-test-virtqemud/libvirt/virtqemud-sock`
- **SSH port:** 22231 (host) → 22 (guest), added via `hostfwd_add` after VM starts
- **SSH key:** `/tmp/bootc-from-existing-root/id_ed25519`
- **VM disk:** `/tmp/bootc-from-existing-root/disk.qcow2` (20GB, btrfs)
- **Base image:** `quay.io/containerdisks/fedora:44`
- **bootc binary:** built with `cargo build --profile=thin -p bootc`, copied to VM at `/usr/local/bin/bootc`
- **Watchdog:** disabled in VM XML (`<watchdog model='itco' action='none'/>`) to prevent resets during heavy I/O
- **Container storage driver:** VFS (not overlay) to avoid overlay-on-btrfs issues
- **Build libs:** `PKG_CONFIG_PATH=/tmp/bootc-build-libs/pkgconfig` required for glib-2.0

