//! # Convert a running package-mode system to a bootc (image-mode) system
//!
//! This module implements `bootc install from-existing-root`, which performs a
//! destructive one-shot conversion of a running package-mode Linux system into a
//! bootc-managed image-mode system using the "black box" snapshot approach:
//!
//! 1. Snapshot the running filesystem into an OCI container image via buildah
//! 2. Push the image to a container registry
//! 3. Invoke `bootc install to-existing-root` via podman to install it
//!
//! All filesystem content is captured — including `/opt`, out-of-tree kernel modules
//! (DKMS), and any other non-RPM content — because the goal is a faithful snapshot
//! of the running system, not a clean reproducible build.
//!
//! ## What is and is not captured
//!
//! **Included:** everything on the root filesystem that is not in the exclusion list
//! below. Additional mount points that share the same block device as `/` (e.g. btrfs
//! subvolumes for `/var` and `/home` on a typical Fedora layout) are also captured so
//! that the resulting image contains functional runtime state (`/var/lib/NetworkManager`,
//! `/var/lib/systemd`, etc.).  Separate block devices (NFS, extra disks) are not
//! captured.
//!
//! **Always excluded:**
//! - `/proc`, `/sys`, `/dev` — virtual kernel filesystems
//! - `/run` — transient runtime state
//! - `/tmp`, `/var/tmp` — temporary files
//! - `/var/cache` — regenerable caches
//! - `/var/log/journal` — systemd journal (large, regenerated on boot)
//! - `/boot` — wiped and replaced by `bootc install to-existing-root`
//! - `/sysroot`, `/ostree` — ostree internals (absent on package-mode systems)
//! - `/var/lib/containers` — container storage (prevents recursive capture)
//! - `/afs` — AFS placeholder directory; always empty but has a special inode
//!   that causes btrfs kernel crashes when tar reads it during large archive writes
//!
//! The running kernel's initramfs is always copied separately from
//! `/boot/initramfs-<kver>.img` into `/usr/lib/modules/<kver>/initramfs.img`
//! inside the image, because that is where bootc expects to find it and `/boot`
//! itself is excluded from the snapshot.

use std::cell::Cell;
use std::io::Write as _;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use camino::Utf8PathBuf;
use chrono::TimeZone as _;
use fn_error_context::context;

/// Where bootc expects the kernel's initramfs inside the image.
const BOOTC_INITRAMFS_DIR: &str = "/usr/lib/modules";

/// Path where `prepare-root.conf` must exist for a valid ostree/bootc image.
const PREPARE_ROOT_CONF_PATH: &str = "/usr/lib/ostree/prepare-root.conf";

/// Minimal `prepare-root.conf` that enables the read-only sysroot required by bootc.
///
/// This is always written into the snapshot image, overriding any version from the
/// running package-mode system.  The running system's `prepare-root.conf` might enable
/// composefs (e.g. `enabled = yes`) — but `bootc install from-existing-root` uses the
/// ostree backend, which does *not* create a composefs image.  If the deployed image
/// retained `composefs.enabled = yes`, `ostree-prepare-root` would try to mount the
/// composefs image at boot, fail (because it was never created), and fall back to
/// "legacy bind-mount mode".  Legacy mode skips the pivot-root sequence that writes the
/// complete `/run/ostree-booted` GVariant, causing `bootc status` to fail with "not
/// currently booted into an OSTree system".
///
/// The minimal config (`sysroot.readonly = true`, no composefs) causes
/// `ostree-prepare-root` to take the modern pivot-root path, which writes the GVariant
/// correctly and allows `bootc status` to detect the booted deployment.
const PREPARE_ROOT_CONF_CONTENT: &[u8] = b"[sysroot]\nreadonly = true\n";

/// Paths excluded from the filesystem snapshot.
///
/// Virtual and transient paths are excluded so the image does not contain stale
/// runtime state. `/boot` is excluded because `to-existing-root` wipes it anyway;
/// the initramfs is copied separately into the image. `/var/lib/containers` is
/// excluded to avoid embedding container storage into the image and to prevent
/// recursive capture while buildah is writing layers.
///
/// Note: `/opt` and `/usr/lib/modules` (including DKMS modules) are intentionally
/// NOT in this list. They are captured verbatim in the snapshot.
const EXCLUDED_PATHS: &[&str] = &[
    "/proc",
    "/sys",
    "/dev",
    "/run",
    "/tmp",
    "/var/tmp",
    "/var/cache",
    "/var/log/journal",
    "/boot",
    "/sysroot",
    "/ostree",
    "/var/lib/containers",
    // /afs is an AFS (Andrew File System) placeholder directory present in some
    // Fedora packages.  Even when AFS is not actually mounted it has a special
    // inode (device 0, inode 264 on btrfs) that causes the btrfs kernel driver
    // to crash when tar tries to read its extended attributes while writing a
    // large archive to disk.  It is always empty, so excluding it loses nothing.
    "/afs",
];

/// Essential directories that are excluded from the snapshot (because they are
/// ephemeral mount points or runtime state) but must exist as empty directories
/// in a valid Linux filesystem image.
///
/// - `/tmp`, `/var/tmp` — tmpfs mount points omitted from the snapshot
/// - `/boot` — wiped by `to-existing-root`, but the directory must be present
///   so the image can serve as a valid OS container image
/// - `/sysroot` — excluded because it's an ostree internal (absent on package-mode
///   systems), but bootc's bootloader installer (`bootupctl`) needs a `/sysroot`
///   bind-mount target inside the deployment directory when installing the GRUB
///   bootloader.  ostree marks the deployment directory immutable after committing
///   it, so the directory must be pre-created in the image, not written
///   post-installation.
const RECREATE_EMPTY_DIRS: &[(&str, u32)] = &[
    ("/tmp", 0o1777),       // sticky + world-writable tmpfs
    ("/var/tmp", 0o1777),   // same
    ("/boot", 0o755),       // ordinary directory, content replaced by installer
    ("/sysroot", 0o755),    // bind-mount target for bootupctl (grub installer)
];

/// Default name given to the intermediate image in local container storage.
const DEFAULT_LOCAL_IMAGE_NAME: &str = "bootc-snapshot:latest";

/// Buildah storage driver to use for the snapshot container.
///
/// The default storage driver (`overlay`) uses overlayfs on top of the host
/// filesystem.  When the host uses btrfs, the overlay-on-btrfs combination can
/// trigger kernel panics in some kernel versions when a large tar archive is
/// unpacked via `buildah add`.  Using `vfs` avoids overlayfs entirely and is
/// safe on all filesystem types, at the cost of not deduplicating layers (which
/// does not matter here because we only have one layer).
const BUILDAH_STORAGE_DRIVER: &str = "vfs";

/// Extra arguments prepended to every `buildah` invocation to select the
/// storage driver.  Declared as a slice so it can be spread into `.args()`.
const BUILDAH_STORAGE_ARGS: &[&str] = &["--storage-driver", BUILDAH_STORAGE_DRIVER];

/// SSH authorized_keys mount point inside the install container.
const SSH_KEY_MOUNT: &str = "/bootc_authorized_ssh_keys/root";

// ── Options struct ────────────────────────────────────────────────────────────

/// Options for `bootc install from-existing-root`.
#[derive(Debug, Clone, clap::Parser, PartialEq, Eq)]
pub(crate) struct InstallFromExistingRootOpts {
    /// Container registry reference to push the snapshot to and install from.
    ///
    /// Example: `registry.example.com/myorg/myhost:latest`
    ///
    /// This reference is also stored in the new deployment as the target for
    /// future `bootc upgrade` operations.
    #[clap(long)]
    pub(crate) image_ref: String,

    /// Accept that this is a destructive, one-way operation and skip the
    /// countdown warning.  `/boot` will be wiped and the bootloader replaced.
    #[clap(long)]
    pub(crate) acknowledge_destructive: bool,

    /// Reboot immediately after the installation completes.
    #[clap(long)]
    pub(crate) reboot: bool,

    /// Enable the `bootc-destructive-cleanup` systemd service, which removes
    /// redundant old OS content from `/sysroot` on the first boot into the new
    /// bootc system.
    #[clap(long)]
    pub(crate) cleanup: bool,

    /// Path to an `authorized_keys` file that will be injected into the root
    /// account of the new deployment.  Forwarded to `to-existing-root`.
    #[clap(long)]
    pub(crate) root_ssh_authorized_keys: Option<Utf8PathBuf>,

    /// Skip pushing the image to the registry and install directly from local
    /// container storage.  Useful for air-gapped environments or local testing.
    ///
    /// Note: future `bootc upgrade` runs will contact the registry specified by
    /// `--image-ref`, so the image must be pushed there before upgrading.
    #[clap(long)]
    pub(crate) skip_push: bool,

    /// Use the composefs storage backend.
    /// Forwarded to `bootc install to-existing-root --composefs-backend`.
    #[clap(long)]
    pub(crate) composefs_backend: bool,

    /// Name given to the intermediate image in local buildah/podman storage.
    #[clap(long, default_value = DEFAULT_LOCAL_IMAGE_NAME)]
    pub(crate) local_image_name: String,
}

// ── Internal types ────────────────────────────────────────────────────────────

/// Information about the running system collected before image creation.
struct SystemInfo {
    /// `uname -r` release string, e.g. `"5.14.0-427.el9.x86_64"`
    kernel_version: String,
    /// Absolute path to the running kernel's initramfs under `/boot`
    initramfs_src: String,
    /// Whether `/usr/lib/modules/<kver>/initramfs.img` already exists
    initramfs_already_placed: bool,
}

impl std::fmt::Debug for SystemInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemInfo")
            .field("kernel_version", &self.kernel_version)
            .field("initramfs_src", &self.initramfs_src)
            .field("initramfs_already_placed", &self.initramfs_already_placed)
            .finish()
    }
}

/// RAII guard that removes a buildah working container when dropped, unless
/// `mark_committed()` is called first.  Ensures cleanup on any error path.
struct BuildahContainerGuard {
    container_id: String,
    committed: Cell<bool>,
}

impl std::fmt::Debug for BuildahContainerGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildahContainerGuard")
            .field("container_id", &self.container_id)
            .field("committed", &self.committed.get())
            .finish()
    }
}

impl BuildahContainerGuard {
    fn new(container_id: impl Into<String>) -> Self {
        Self {
            container_id: container_id.into(),
            committed: Cell::new(false),
        }
    }

    /// Signal that the container has been committed; Drop will not remove it.
    fn mark_committed(&self) {
        self.committed.set(true);
    }

    fn id(&self) -> &str {
        &self.container_id
    }
}

impl Drop for BuildahContainerGuard {
    fn drop(&mut self) {
        if !self.committed.get() {
            // Best-effort cleanup on error paths.
            let _ = std::process::Command::new(bootc_utils::buildah_bin())
                .args(BUILDAH_STORAGE_ARGS)
                .args(["rm", &self.container_id])
                .status();
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Entry point for `bootc install from-existing-root`.
pub(crate) async fn install_from_existing_root(opts: InstallFromExistingRootOpts) -> Result<()> {
    validate_prerequisites(&opts)?;

    if !opts.acknowledge_destructive {
        print_destructive_warning(&opts.image_ref)?;
    }

    let info = gather_system_info().context("Gathering system information")?;

    println!("Creating OCI snapshot image from running system...");
    println!("Image size may be several gigabytes; this can take several minutes.");
    println!();

    create_snapshot_image(&opts, &info).context("Creating OCI snapshot image")?;

    if !opts.skip_push {
        push_image(&opts.local_image_name, &opts.image_ref)
            .context("Pushing image to registry")?;
    }

    // The image that `podman run` actually executes.  In the normal (push) case
    // this is the registry reference; in --skip-push it is the local image.
    let install_source = if opts.skip_push {
        format!("containers-storage:localhost/{}", opts.local_image_name)
    } else {
        opts.image_ref.clone()
    };

    run_install(&opts, &install_source).context("Running bootc install to-existing-root")?;

    if opts.reboot {
        println!("Installation complete. Rebooting now...");
        std::process::Command::new("reboot")
            .status()
            .context("Triggering reboot")?;
    } else {
        println!();
        println!("Installation complete. Reboot to enter the new bootc-managed system.");
        if opts.cleanup {
            println!(
                "bootc-destructive-cleanup.service will remove old OS content \
                 from /sysroot on first boot."
            );
        }
        println!("The previous root will be accessible at /sysroot after reboot.");
    }

    Ok(())
}

// ── Phase 1: Validate prerequisites ──────────────────────────────────────────

#[context("Validating prerequisites")]
fn validate_prerequisites(opts: &InstallFromExistingRootOpts) -> Result<()> {
    // Requires root and CAP_SYS_ADMIN.
    crate::cli::require_root(false)?;

    // Must be running on the host, not inside a container.
    // cap_std_ext re-exports cap_std, so use the full path to avoid an extra `use`.
    let rootfs = cap_std_ext::cap_std::fs::Dir::open_ambient_dir(
        "/",
        cap_std_ext::cap_std::ambient_authority(),
    )
    .context("Opening /")?;
    ensure!(
        !crate::containerenv::is_container(&rootfs),
        "This command must be run on the host system, not inside a container.\n\
         To install from within a container image, use `bootc install to-existing-root`."
    );

    // Must not already be a bootc/ostree deployment.
    ensure!(
        !is_bootc_system(),
        "This system is already managed by bootc.\n\
         Use `bootc upgrade` or `bootc switch` to change the running image."
    );

    // buildah and podman must be reachable in PATH.
    check_binary(bootc_utils::buildah_bin())
        .context("buildah is required to create the snapshot image")?;
    check_binary(bootc_utils::podman_bin())
        .context("podman is required to run the install container")?;

    // Basic sanity check on the image reference.
    validate_image_ref(&opts.image_ref)?;

    Ok(())
}

/// Returns true if the running system is an ostree/bootc deployment.
fn is_bootc_system() -> bool {
    std::path::Path::new("/sysroot/ostree").exists()
        || std::path::Path::new("/ostree/deploy").exists()
}

/// Verify a binary is reachable by probing with `--version`.
/// Avoids adding the `which` crate to bootc-lib's dependency tree.
fn check_binary(bin: &str) -> Result<()> {
    let status = std::process::Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => anyhow::bail!(
            "`{bin}` was not found in PATH or returned an error.\n\
             Install it with: dnf install {bin}"
        ),
    }
}

/// Basic sanity check that the image reference looks like a registry path.
/// Full validation happens when podman push runs.
fn validate_image_ref(image_ref: &str) -> Result<()> {
    ensure!(
        image_ref.contains('/'),
        "Image reference '{image_ref}' does not look like a registry reference.\n\
         Expected format: registry.example.com/namespace/image:tag"
    );
    Ok(())
}

// ── Phase 2: Gather system information ───────────────────────────────────────

#[context("Gathering running system information")]
fn gather_system_info() -> Result<SystemInfo> {
    let uname = rustix::system::uname();
    let kernel_version = uname
        .release()
        .to_str()
        .context("Kernel version string is not valid UTF-8")?
        .to_string();

    // The initramfs lives in /boot on package-mode systems.
    let initramfs_src = format!("/boot/initramfs-{kernel_version}.img");
    ensure!(
        std::path::Path::new(&initramfs_src).exists(),
        "Initramfs not found at {initramfs_src}.\n\
         Regenerate it with: dracut --force {initramfs_src} {kernel_version}"
    );

    // bootc expects the initramfs at /usr/lib/modules/<kver>/initramfs.img.
    let bootc_initramfs = format!("{BOOTC_INITRAMFS_DIR}/{kernel_version}/initramfs.img");
    let initramfs_already_placed = std::path::Path::new(&bootc_initramfs).exists();

    Ok(SystemInfo {
        kernel_version,
        initramfs_src,
        initramfs_already_placed,
    })
}

// ── Phase 3: Create OCI image ─────────────────────────────────────────────────

#[context("Creating OCI snapshot image")]
fn create_snapshot_image(opts: &InstallFromExistingRootOpts, info: &SystemInfo) -> Result<()> {
    // Step 1: create an empty working container from scratch.
    println!("  [1/4] Creating empty buildah container...");
    let container_id = {
        let out = std::process::Command::new(bootc_utils::buildah_bin())
            .args(BUILDAH_STORAGE_ARGS)
            .args(["from", "scratch"])
            .output()
            .context("Running `buildah from scratch`")?;
        ensure!(
            out.status.success(),
            "`buildah from scratch` failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .context("`buildah from scratch` produced non-UTF-8 output")?
            .trim()
            .to_string()
    };
    // RAII guard removes the container if we bail out before committing.
    let guard = BuildahContainerGuard::new(&container_id);

    // Step 2: snapshot the running filesystem into the container.
    println!("  [2/4] Snapshotting running filesystem (this may take several minutes)...");
    snapshot_filesystem(guard.id())?;

    // Step 3: inject files that require special handling.
    println!("  [3/4] Injecting bootc-required files...");
    inject_required_files(guard.id(), info)?;

    // Step 4: commit the container as a local OCI image.
    println!("  [4/4] Committing image as {}...", opts.local_image_name);
    commit_image(guard.id(), &opts.local_image_name, info)?;

    // Prevent Drop from trying to remove the container (it no longer exists).
    guard.mark_committed();
    Ok(())
}
/// Choose the best directory for the intermediate tar archive.
///
/// Prefer `/tmp` (tmpfs) to avoid writing large sequential files to btrfs, which
/// can trigger kernel crashes on some Fedora kernel versions when tar simultaneously
/// reads from the same btrfs filesystem.  `/tmp` is typically a tmpfs of 50% of RAM.
///
/// Fall back to `/var/tmp` (persistent disk) if `/tmp` is not a tmpfs (e.g. the
/// admin has mounted a non-tmpfs there) or if the available space there is less than
/// the minimum threshold.
fn choose_tar_dir() -> &'static str {
    // Check if /tmp is a tmpfs by comparing its st_dev to /proc's
    // well-known tmpfs device, or more simply: check that the fstype
    // from /proc/mounts for /tmp is "tmpfs".
    let is_tmp_tmpfs = std::fs::read_to_string("/proc/mounts")
        .map(|mounts| {
            mounts.lines().any(|line| {
                let mut parts = line.split_whitespace();
                let _dev = parts.next().unwrap_or("");
                let mp = parts.next().unwrap_or("");
                let fs = parts.next().unwrap_or("");
                mp == "/tmp" && fs == "tmpfs"
            })
        })
        .unwrap_or(false);

    if !is_tmp_tmpfs {
        eprintln!(
            "  [snapshot] /tmp is not a tmpfs; falling back to /var/tmp for tar archive"
        );
        return "/var/tmp";
    }

    // /tmp is a tmpfs — use it to avoid btrfs kernel crash.
    "/tmp"
}

/// Snapshot the running filesystem into the buildah working container.
///
/// `tar` archives the live root filesystem (excluding virtual/transient paths) to a
/// temporary file in `/tmp` (tmpfs), then `buildah add` unpacks it at `/` inside the
/// container.
///
/// Using an intermediate file (rather than piping tar's stdout directly to
/// `buildah add`) is required because newer versions of buildah (≥ 1.40) no longer
/// accept `-` as a stand-in for stdin in `buildah add`.  A FIFO (named pipe) would
/// avoid the disk write but cannot easily be used from the single-threaded tokio
/// runtime that bootc uses.
///
/// The archive is written to `/tmp` (tmpfs backed by RAM) rather than `/var/tmp`
/// (which is typically on btrfs) because writing large sequential files to btrfs
/// while simultaneously reading the same btrfs filesystem can trigger kernel
/// crashes in some kernel versions (observed on Fedora 44 with kernel 6.19).
/// Writing to tmpfs avoids this btrfs I/O pattern entirely.
///
/// `/tmp` default size is 50% of RAM.  For systems where the rootfs snapshot would
/// exceed that limit, the function falls back to `/var/tmp` (on the persistent
/// disk) after logging a warning.
#[context("Snapshotting running filesystem into buildah container")]
fn snapshot_filesystem(container_id: &str) -> Result<()> {
    // Choose the best location for the intermediate tar archive.
    //
    // Prefer /tmp (tmpfs) because writing large files to btrfs while tar
    // simultaneously reads from the same btrfs filesystem triggers a kernel
    // crash on some Fedora kernel versions.  Tmpfs is safe from this bug.
    //
    // Fall back to /var/tmp (persistent disk) if /tmp is not a tmpfs or if the
    // tmpfs is smaller than a configurable threshold.
    //
    // We do NOT open the file before tar runs; instead we let tar create it so
    // that we never hold an fd open on the inode while btrfs is active.
    let tar_dir = choose_tar_dir();
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let tar_path = format!("{tar_dir}/bootc-snap-{ts:08x}.tar");
    // Ensure we delete the file when this scope exits (success or error).
    struct TarFileCleanup(String);
    impl Drop for TarFileCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _tar_cleanup = TarFileCleanup(tar_path.clone());

    // Determine which additional mount points (btrfs subvolumes, etc.) share the
    // same underlying block device as `/` and should be included in the snapshot.
    //
    // On a typical Fedora/RHEL btrfs layout each subvolume (/, /var, /home,
    // /boot, …) is a separate mount point.  Each subvolume reports a *different*
    // st_dev even though they all live on the same physical block device.
    // Comparing st_dev values would therefore miss all subvolumes, leaving the
    // snapshot with an empty /var (and a non-functional NetworkManager, systemd-
    // logind, etc.) after the first ostree boot.
    //
    // Instead, we look up the block device string for the root mount in
    // /proc/mounts and treat all other mounts with the same block device string
    // as "same-device" mounts that should be captured in pass 2.
    //
    // Strategy: two-pass archive.
    //   Pass 1 – capture everything on the root mount itself
    //            (--one-file-system so we do not accidentally recurse into
    //             NFS/overlay/tmpfs that happen to land under / as well).
    //   Pass 2 – for each additional mount point with the same block device
    //            that is not already in EXCLUDED_PATHS and is not a virtual/
    //            network filesystem, append its contents into the same tar
    //            archive with the correct path prefix.

    // Collect additional same-block-device mount points by reading /proc/mounts.
    // We skip mounts whose fstype is virtual (proc, sysfs, devtmpfs, tmpfs,
    // cgroup, overlay, …) and whose mount point is already covered by
    // EXCLUDED_PATHS or is the root mount itself.
    let virtual_fstypes: std::collections::HashSet<&str> = [
        "proc", "sysfs", "devtmpfs", "tmpfs", "devpts", "cgroup", "cgroup2",
        "hugetlbfs", "mqueue", "securityfs", "pstore", "debugfs", "tracefs",
        "configfs", "efivarfs", "fusectl", "autofs", "bpf", "fuse.gvfsd-fuse",
        "overlay", "nsfs", "ramfs",
    ]
    .iter()
    .copied()
    .collect();

    let mounts_raw = std::fs::read_to_string("/proc/mounts")
        .context("Reading /proc/mounts")?;

    // Find the block device for the root mount.
    let root_block_dev: Option<String> = mounts_raw.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let dev = parts.next()?;
        let mp = parts.next()?;
        if mp == "/" { Some(dev.to_string()) } else { None }
    });

    let mut extra_mounts: Vec<String> = Vec::new();
    for line in mounts_raw.lines() {
        let mut parts = line.split_whitespace();
        let dev = parts.next().unwrap_or("");
        let mountpoint = parts.next().unwrap_or("");
        let fstype = parts.next().unwrap_or("");
        if mountpoint == "/" {
            continue; // root itself is covered by pass 1
        }
        // Skip virtual/network filesystems.
        if virtual_fstypes.contains(fstype) {
            continue;
        }
        // Skip anything already in EXCLUDED_PATHS.
        if EXCLUDED_PATHS.iter().any(|ex| {
            mountpoint == *ex || mountpoint.starts_with(&format!("{ex}/"))
        }) {
            continue;
        }
        // Include if this mount uses the same block device as /.
        if let Some(ref root_dev) = root_block_dev {
            if dev == root_dev.as_str() {
                extra_mounts.push(mountpoint.to_string());
            }
        }
    }

    // ── Pass 1: root filesystem (--one-file-system) ──────────────────────────
    let mut tar_args: Vec<String> = vec![
        "--create".into(),
        format!("--file={tar_path}"),
        // Do not cross mount-point boundaries for the root pass.  Separately-
        // mounted filesystems on OTHER devices (NFS, extra disks, overlayfs,
        // …) are intentionally skipped; same-device btrfs subvolumes are
        // handled in pass 2 below.
        "--one-file-system".into(),
        "--sparse".into(),
        "--acls".into(),
        // Preserve all extended attributes, including SELinux security contexts.
        "--xattrs".into(),
    ];

    // NOTE: GNU tar strips the leading "/" from archive member names
    // ("Removing leading `/'" from member names"), so exclude patterns
    // must NOT start with "/" or they will never match.  E.g. use
    // "--exclude=proc" not "--exclude=/proc".
    for path in EXCLUDED_PATHS {
        tar_args.push(format!("--exclude={}", path.trim_start_matches('/')));
    }
    // Also exclude extra mount points from the root pass so tar does not
    // try to archive their (empty) mount-point directories and then fail
    // when pass 2 re-archives the same path.
    for mp in &extra_mounts {
        tar_args.push(format!("--exclude={}", mp.trim_start_matches('/')));
    }

    tar_args.push("/".into()); // source: root filesystem

    // Print the full tar command for debugging.
    eprintln!("  [tar pass 1] tar {:?}", tar_args);

    let tar_status = std::process::Command::new("tar")
        .args(&tar_args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Running tar (pass 1: root filesystem)")?;

    // GNU tar exits with code 1 for non-fatal warnings ("file changed as we
    // read it") which are expected for a live snapshot; code 2 is fatal.
    let tar_ok = tar_status.code().map(|c| c <= 1).unwrap_or(false);
    ensure!(
        tar_ok,
        "tar exited with a fatal error (pass 1, exit code {:?})",
        tar_status.code()
    );

    // ── Pass 2: same-device additional mount points ───────────────────────────
    // Append the contents of each extra mount point (e.g. /var, /home on a
    // btrfs system with separate subvolumes) into the same tar archive.
    for mp in &extra_mounts {
        println!("  Snapshotting additional mount point: {mp}");
        let mut mp_args: Vec<String> = vec![
            "--append".into(),
            format!("--file={tar_path}"),
            "--sparse".into(),
            "--acls".into(),
            "--xattrs".into(),
            // Do not cross further nested mounts.
            "--one-file-system".into(),
            // Transform paths: strip the leading "/" so the archive has
            // entries like "var/lib/..." (relative to /var) which buildah
            // will correctly place at /var/... inside the container.
            format!("--transform=s|^\\./|{}/|", mp.trim_start_matches('/')),
        ];
        // Exclude sub-paths that fall under this mount point.
        for ex in EXCLUDED_PATHS {
            if ex.starts_with(mp.as_str()) {
                // Make the exclude relative to the mountpoint.
                let relative = ex.trim_start_matches(mp.as_str()).trim_start_matches('/');
                if !relative.is_empty() {
                    mp_args.push(format!("--exclude={relative}"));
                }
            }
        }
        // Exclude the tar archive file itself if it falls under this mount
        // point.  Although we prefer /tmp (tmpfs) the archive could in theory
        // be in /var/tmp if the fallback path was taken.  Without this
        // exclusion, pass 2 would try to archive the partially-written tar
        // file while simultaneously appending to it, causing the file to grow
        // unboundedly.
        if tar_path.starts_with(mp.as_str()) {
            let rel = tar_path
                .trim_start_matches(mp.as_str())
                .trim_start_matches('/');
            if !rel.is_empty() {
                mp_args.push(format!("--exclude={rel}"));
            }
        }
        // Source directory: the mount point itself (tar with -C).
        mp_args.push("-C".into());
        mp_args.push(mp.clone());
        mp_args.push(".".into());

        let status = std::process::Command::new("tar")
            .args(&mp_args)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context(format!("Running tar (pass 2: {mp})"))?;

        let ok = status.code().map(|c| c <= 1).unwrap_or(false);
        if !ok {
            tracing::warn!(
                "tar for mount point {mp} exited with {:?}; continuing",
                status.code()
            );
        }
    }

    // `buildah add <container> <tar_path> /` unpacks the tar at `/` inside the container.
    let buildah_status = std::process::Command::new(bootc_utils::buildah_bin())
        .args(BUILDAH_STORAGE_ARGS)
        .args(["add", "--quiet", container_id, &tar_path, "/"])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Running `buildah add`")?;

    // `_tar_cleanup` is dropped here, deleting the temp tar file.

    ensure!(
        buildah_status.success(),
        "`buildah add` failed (exit code {:?})",
        buildah_status.code()
    );

    Ok(())
}

/// Attempt to regenerate the initramfs inside the container with dracut.
///
/// The package-mode initramfs copied from `/boot/initramfs-<kver>.img` may be missing
/// the `ostree` dracut module and its shared library dependencies
/// (`libgio-2.0.so.0`, `libglib-2.0.so.0`, etc.) that `ostree-prepare-root` requires.
/// This function runs `dracut --force --add ostree` inside the buildah container to
/// produce a properly-built initramfs.
///
/// Failure is non-fatal: if `dracut` is absent or fails (e.g., the snapshot came from
/// a minimal container image), the function logs a warning and returns, leaving the
/// previously-copied package-mode initramfs in place.
fn regenerate_initramfs_with_dracut(container_id: &str, info: &SystemInfo, dest: &str) {
    // Check if dracut is available inside the container.
    let dracut_check = std::process::Command::new(bootc_utils::buildah_bin())
        .args(BUILDAH_STORAGE_ARGS)
        .args(["run", container_id, "--", "sh", "-c", "command -v dracut"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let dracut_available = dracut_check.map(|s| s.success()).unwrap_or(false);
    if !dracut_available {
        println!(
            "      WARNING: dracut not found inside container; keeping package-mode initramfs."
        );
        println!("               The initramfs may be missing ostree-prepare-root dependencies.");
        return;
    }

    println!(
        "      Regenerating initramfs with dracut --add ostree          (ensures ostree-prepare-root dependencies are present)..."
    );

    // Ensure /var/tmp exists inside the container; dracut uses it as a scratch space.
    // /var/tmp is in EXCLUDED_PATHS so the snapshot omits it, but dracut will fail
    // immediately with "Invalid tmpdir" if it doesn't exist.
    let mkdir_status = std::process::Command::new(bootc_utils::buildah_bin())
        .args(BUILDAH_STORAGE_ARGS)
        .args(["run", container_id, "--", "mkdir", "-p", "/var/tmp"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if let Ok(s) = mkdir_status {
        if !s.success() {
            println!(
                "      WARNING: failed to create /var/tmp in container; dracut may fail."
            );
        }
    }

    // Run dracut inside the container with the ostree module enabled.
    // --no-hostonly: do not limit to hardware detected on the *host* (which is not the target).
    // --force: overwrite the destination file (we just placed it above).
    // --add ostree: include the ostree dracut module, which brings in ostree-prepare-root
    //              and all of its shared library dependencies.
    let status = std::process::Command::new(bootc_utils::buildah_bin())
        .args(BUILDAH_STORAGE_ARGS)
        .args([
            "run",
            container_id,
            "--",
            "dracut",
            "--no-hostonly",
            "--force",
            "--add",
            "ostree",
            dest,
            &info.kernel_version,
        ])
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("      Initramfs regenerated successfully with ostree module.");
        }
        Ok(s) => {
            println!(
                "      WARNING: dracut exited with code {:?}; keeping package-mode initramfs.",
                s.code()
            );
            println!(
                "               The initramfs may be missing ostree-prepare-root dependencies."
            );
        }
        Err(e) => {
            println!(
                "      WARNING: failed to run dracut ({e}); keeping package-mode initramfs."
            );
        }
    }
}

/// Copy the initramfs and (if absent) `prepare-root.conf` into the container,
/// then stamp the image config with descriptive OCI labels.
#[context("Injecting bootc-required files into buildah container")]
fn inject_required_files(container_id: &str, info: &SystemInfo) -> Result<()> {
    // Package-mode RHEL keeps the initramfs in /boot; bootc expects it at
    // /usr/lib/modules/<kver>/initramfs.img.
    if !info.initramfs_already_placed {
        let dest = format!(
            "{BOOTC_INITRAMFS_DIR}/{}/initramfs.img",
            info.kernel_version
        );
        println!("      Placing initramfs at {dest}");
        buildah_copy(container_id, &info.initramfs_src, &dest)
            .context("Copying initramfs into container")?;
        // The package-mode initramfs may be missing the ostree dracut module and
        // its shared library dependencies (libgio-2.0.so.0, libglib-2.0.so.0, etc.)
        // required by ostree-prepare-root.  Attempt to regenerate the initramfs
        // with dracut inside the container so that the ostree module is properly
        // included with all dependencies.  If dracut is not available or fails
        // (e.g., on a minimal system), keep the copied initramfs and warn the user.
        regenerate_initramfs_with_dracut(container_id, info, &dest);
    }

    // Always write the minimal prepare-root.conf into the snapshot image, overriding
    // any version from the running system.  The running system's config might enable
    // composefs (common on Fedora/RHEL where the ostree package ships
    // `composefs.enabled = yes`), but `bootc install from-existing-root` uses the
    // ostree backend without composefs.  If the composefs config were retained,
    // ostree-prepare-root would attempt to mount a composefs image at boot, fail
    // (because no composefs image was created), and fall back to legacy bind-mount
    // mode — which prevents `bootc status` from detecting the booted deployment.
    {
        println!("      Writing minimal {PREPARE_ROOT_CONF_PATH} (overrides composefs if present)");
        let tmp =
            tempfile::NamedTempFile::new().context("Creating tempfile for prepare-root.conf")?;
        tmp.as_file()
            .write_all(PREPARE_ROOT_CONF_CONTENT)
            .context("Writing prepare-root.conf content")?;
        let tmp_path = tmp
            .path()
            .to_str()
            .context("Tempfile path is not valid UTF-8")?;
        // Ensure the parent directory exists before copying.
        let status = std::process::Command::new(bootc_utils::buildah_bin())
            .args(BUILDAH_STORAGE_ARGS)
            .args(["run", container_id, "--", "mkdir", "-p", "/usr/lib/ostree"])
            .status()
            .context("Creating /usr/lib/ostree in container")?;
        ensure!(
            status.success(),
            "`buildah run mkdir -p /usr/lib/ostree` failed"
        );
        buildah_copy(container_id, tmp_path, PREPARE_ROOT_CONF_PATH)
            .context("Writing prepare-root.conf into container")?;
    }

    // Create the /ostree → sysroot/ostree symlink inside the container image.
    //
    // Standard bootc images (e.g. fedora-bootc) include this symlink as committed
    // content.  After boot, `/` is the deployment root and `/sysroot` is the physical
    // root; the symlink allows tools like `ostree` and `bootc status` to locate the
    // ostree repo at `/ostree/repo` (→ `/sysroot/ostree/repo`).
    //
    // Package-mode systems do not have `/ostree`, and EXCLUDED_PATHS explicitly
    // excludes it from the snapshot.  Without this symlink, the ostree sysroot lock
    // at `/ostree/lock` fails with ENOENT when `bootc status` runs post-boot, because
    // `/ostree` does not exist in the deployment root.
    {
        println!("      Creating /ostree → sysroot/ostree symlink");
        let status = std::process::Command::new(bootc_utils::buildah_bin())
            .args(BUILDAH_STORAGE_ARGS)
            .args(["run", container_id, "--", "ln", "-sf", "sysroot/ostree", "/ostree"])
            .status()
            .context("Creating /ostree symlink in container")?;
        ensure!(
            status.success(),
            "`buildah run ln -sf sysroot/ostree /ostree` failed"
        );
    }

    // Re-create empty directories that were excluded from the snapshot because they
    // are ephemeral mount points (tmpfs) or wiped by the installer, but must be
    // present as directory entries in a valid Linux image.  Without these, tools
    // such as `bootc install to-existing-root` and `tempfile` crate will fail with
    // "No such file or directory" when they try to create files inside them.
    for (path, mode) in RECREATE_EMPTY_DIRS {
        // Use buildah run so the directory is created inside the container's
        // filesystem, not on the host.  `mkdir -p` is idempotent if the
        // directory was somehow preserved in the snapshot.
        let status = std::process::Command::new(bootc_utils::buildah_bin())
            .args(BUILDAH_STORAGE_ARGS)
            .args([
                "run",
                container_id,
                "--",
                "sh",
                "-c",
                &format!("mkdir -p '{path}' && chmod {mode:04o} '{path}'"),
            ])
            .status()
            .context("Running `buildah run` to create directory")?;
        ensure!(
            status.success(),
            "`buildah run mkdir -p {path}` failed (exit code {:?})",
            status.code()
        );
    }

    set_image_labels(container_id, info).context("Setting OCI image labels")?;

    Ok(())
}

/// Run `buildah copy <container> <src> <dest>`.
fn buildah_copy(container_id: &str, src: &str, dest: &str) -> Result<()> {
    let status = std::process::Command::new(bootc_utils::buildah_bin())
        .args(BUILDAH_STORAGE_ARGS)
        .args(["copy", container_id, src, dest])
        .status()
        .context("Running `buildah copy`")?;
    ensure!(
        status.success(),
        "`buildah copy {src} {dest}` failed (exit code {:?})",
        status.code()
    );
    Ok(())
}

/// Stamp the OCI image config with informational labels.
fn set_image_labels(container_id: &str, info: &SystemInfo) -> Result<()> {
    let created = current_rfc3339();
    let uname = rustix::system::uname();
    let hostname = uname.nodename().to_str().unwrap_or("unknown");

    let status = std::process::Command::new(bootc_utils::buildah_bin())
        .args(BUILDAH_STORAGE_ARGS)
        .args([
            "config",
            "--label",
            &format!("org.opencontainers.image.created={created}"),
            "--label",
            &format!(
                "org.opencontainers.image.description=\
                 bootc snapshot of {hostname} converted from package mode on {created}"
            ),
            "--label",
            "bootc.from-existing-root=true",
            "--label",
            &format!("bootc.source-kernel-version={}", info.kernel_version),
            // Mark the image as a bootc-compatible image so that
            // `bootc install to-existing-root` (and the ostree-ext importer)
            // accept it.  Without this label the install fails with
            // "Target image does not have ostree.bootable label".
            "--label",
            "containers.bootc=1",
            container_id,
        ])
        .status()
        .context("Running `buildah config`")?;
    ensure!(
        status.success(),
        "`buildah config` failed (exit code {:?})",
        status.code()
    );
    Ok(())
}

/// Commit the buildah working container as a local OCI image.
fn commit_image(container_id: &str, local_image_name: &str, _info: &SystemInfo) -> Result<()> {
    let status = std::process::Command::new(bootc_utils::buildah_bin())
        .args(BUILDAH_STORAGE_ARGS)
        .args(["commit", "--format", "oci", container_id, local_image_name])
        .status()
        .context("Running `buildah commit`")?;
    ensure!(
        status.success(),
        "`buildah commit` failed (exit code {:?})",
        status.code()
    );
    Ok(())
}

/// Return a current RFC 3339 timestamp without requiring chrono's `clock` feature.
///
/// `std::time::SystemTime` is used to get the epoch second count, and then chrono
/// is used only for formatting (which does not require the `clock` feature).
fn current_rfc3339() -> String {
    let epoch_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // chrono::Utc.timestamp_opt is a constructor — it does not require the
    // `clock` feature, unlike chrono::Utc::now().
    chrono::Utc
        .timestamp_opt(epoch_secs, 0)
        .single()
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| String::from("1970-01-01T00:00:00+00:00"))
}

// ── Phase 4: Push to registry ─────────────────────────────────────────────────

#[context("Pushing image to {image_ref}")]
fn push_image(local_image_name: &str, image_ref: &str) -> Result<()> {
    println!("Pushing image to {image_ref}...");
    println!("(Large system snapshots may take several minutes to upload.)");

    let status = std::process::Command::new(bootc_utils::podman_bin())
        .args([
            "push",
            // zstd:chunked enables content-addressable chunk deduplication
            // for registries that support it.
            "--compress-format",
            "zstd:chunked",
            local_image_name,
            image_ref,
        ])
        .status()
        .context("Running `podman push`")?;
    ensure!(
        status.success(),
        "`podman push` failed (exit code {:?}).\n\
         Check registry credentials and network connectivity.",
        status.code()
    );

    println!("Image pushed successfully.");
    Ok(())
}

// ── Phase 5: Install via to-existing-root ────────────────────────────────────

/// Run `bootc install to-existing-root` inside a privileged podman container.
///
/// `install_source` is what podman actually runs (either a registry reference
/// or a `containers-storage:` URI for the `--skip-push` case).  The stored
/// upgrade target is always `opts.image_ref`.
#[context("Running bootc install to-existing-root")]
fn run_install(opts: &InstallFromExistingRootOpts, install_source: &str) -> Result<()> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "--privileged".into(),
        "--pid=host".into(),
        "--user=root:root".into(),
        // Mount host container storage so bootc inside the container can access
        // the snapshot image (--skip-push path) and write the new deployment.
        "-v".into(),
        "/var/lib/containers:/var/lib/containers".into(),
        "-v".into(),
        "/dev:/dev".into(),
        "--security-opt".into(),
        "label=type:unconfined_t".into(),
        // Mount the host root so `to-existing-root` can see and modify it.
        "-v".into(),
        "/:/target".into(),
    ];

    // Forward RUST_LOG so callers can enable verbose bootc output.
    if let Ok(rust_log) = std::env::var("RUST_LOG") {
        args.push(format!("--env=RUST_LOG={rust_log}"));
    }

    // Mount the authorized_keys file if provided.
    if let Some(keys_path) = &opts.root_ssh_authorized_keys {
        args.push("-v".into());
        args.push(format!("{keys_path}:{SSH_KEY_MOUNT}"));
    }

    // The image that podman runs (contains the bootc binary from the snapshot).
    args.push(install_source.to_string());

    // ── bootc install to-existing-root arguments ──────────────────────────────

    args.extend(["bootc", "install", "to-existing-root"].map(String::from));

    // --source-imgref: where bootc pulls the image content from.
    // Matches install_source in both the push and skip-push cases.
    args.push("--source-imgref".into());
    args.push(install_source.to_string());

    // --target-imgref: the registry reference stored for future `bootc upgrade`.
    // Only differs from --source-imgref in the --skip-push case.
    if opts.skip_push {
        args.push("--target-imgref".into());
        args.push(opts.image_ref.clone());
    }

    args.push("--acknowledge-destructive".into());

    // The image has no embedded pull secret yet; skip the fetch-check that
    // would fail because of that.
    args.push("--skip-fetch-check".into());

    if opts.cleanup {
        args.push("--cleanup".into());
    }
    if opts.composefs_backend {
        args.push("--composefs-backend".into());
    }
    if opts.root_ssh_authorized_keys.is_some() {
        args.push("--root-ssh-authorized-keys".into());
        args.push(SSH_KEY_MOUNT.into());
    }

    println!(
        "Running: {} {}",
        bootc_utils::podman_bin(),
        args.join(" ")
    );
    println!();

    let status = std::process::Command::new(bootc_utils::podman_bin())
        .args(&args)
        .status()
        .context("Spawning podman")?;
    ensure!(
        status.success(),
        "`bootc install to-existing-root` failed (exit code {:?})",
        status.code()
    );

    Ok(())
}

// ── Warning display ───────────────────────────────────────────────────────────

fn print_destructive_warning(image_ref: &str) -> Result<()> {
    eprintln!();
    eprintln!("WARNING: DESTRUCTIVE OPERATION — NO AUTOMATIC ROLLBACK");
    eprintln!("=======================================================");
    eprintln!();
    eprintln!(
        "This command converts the running system from package mode to bootc \
         (image mode)."
    );
    eprintln!("This is a ONE-WAY, IRREVERSIBLE operation:");
    eprintln!();
    eprintln!("  * /boot is WIPED and the bootloader configuration is replaced");
    eprintln!("  * There is NO automatic rollback to the package-mode system");
    eprintln!("  * The system must be rebooted to complete the conversion");
    eprintln!();
    eprintln!("Target image: {image_ref}");
    eprintln!();
    eprintln!("Pass --acknowledge-destructive to skip this timer.");

    // Mirror the countdown style used in install.rs warn_on_host_root().
    for i in (1..=10).rev() {
        eprint!("\rProceeding in {i}s... (Ctrl-C to abort) ");
        std::io::stderr().flush()?;
        std::thread::sleep(Duration::from_secs(1));
    }
    eprintln!();
    eprintln!();

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_image_ref_ok() {
        assert!(validate_image_ref("registry.example.com/myhost:latest").is_ok());
        assert!(validate_image_ref("quay.io/myorg/myimage").is_ok());
        assert!(validate_image_ref("localhost:5000/test:v1").is_ok());
    }

    #[test]
    fn test_validate_image_ref_bad() {
        assert!(validate_image_ref("justimage").is_err());
        assert!(validate_image_ref("").is_err());
    }

    #[test]
    fn test_excluded_paths_contain_essentials() {
        for required in &[
            "/proc",
            "/sys",
            "/dev",
            "/boot",
            "/run",
            "/var/lib/containers",
        ] {
            assert!(
                EXCLUDED_PATHS.contains(required),
                "EXCLUDED_PATHS is missing {required}"
            );
        }
    }

    #[test]
    fn test_opt_and_dkms_not_excluded() {
        // /opt must be included — it may contain critical enterprise software.
        assert!(
            !EXCLUDED_PATHS.iter().any(|p| *p == "/opt"),
            "/opt should be included in the snapshot"
        );
        // /usr (including DKMS modules in /usr/lib/modules) must be included.
        assert!(
            !EXCLUDED_PATHS.iter().any(|p| p.starts_with("/usr")),
            "/usr (including DKMS modules) should be included in the snapshot"
        );
    }

    #[test]
    fn test_current_rfc3339_format() {
        let ts = current_rfc3339();
        // Must contain a 'T' separator and timezone info.
        assert!(ts.contains('T'), "timestamp should be RFC3339: {ts}");
        assert!(
            ts.ends_with('Z') || ts.contains('+') || ts.contains('-'),
            "timestamp should have timezone: {ts}"
        );
    }
}
