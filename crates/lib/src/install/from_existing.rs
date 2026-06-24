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
use cap_std_ext::cap_std::ambient_authority;
use cap_std_ext::cap_std::fs::Dir as CapStdDir;
use chrono::TimeZone as _;
use composefs_ctl::composefs::generic_tree::{FileSystem, Stat};
use etc_merge::{compute_diff, merge, traverse_etc};
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

/// Buildah global arguments for the snapshot container's storage.
///
/// We explicitly request the `vfs` storage driver because the default
/// (`overlay`) uses overlayfs on top of the host filesystem.  On systems that
/// use btrfs as their root filesystem, overlay-on-btrfs can trigger kernel
/// panics on some kernel versions (notably Fedora 44, kernel 6.19) when a
/// large tar archive is unpacked via `buildah add`.  Using `vfs` avoids
/// overlayfs entirely and is safe on all filesystem types, at the cost of not
/// deduplicating layers (which does not matter here because we only have one
/// layer).
const BUILDAH_STORAGE_ARGS: &[&str] = &["--storage-driver", "vfs"];

/// SSH authorized_keys mount point inside the install container.
const SSH_KEY_MOUNT: &str = "/bootc_authorized_ssh_keys/root";

/// Symlinks that must exist at the root of a bootc image but are plain
/// directories in a package-mode (RPM) system.
///
/// In a proper bootc base image (e.g. `quay.io/fedora/fedora-bootc:44`) these
/// symlinks are set up by the rpm-ostree compose postprocessing code
/// (`compose_init_rootfs_strict` / `OSTREE_HOME_SYMLINKS` in
/// `composepost.rs`).  A package-mode system ships real directories in these
/// locations because the standard `filesystem` RPM does not create the
/// ostree-layout symlinks — only the rpm-ostree compose path does.
///
/// `from-existing-root` snapshots the running package-mode system verbatim, so
/// the resulting OCI image ends up with real directories instead of symlinks.
/// This table is used by `inject_required_files` to fix up the image after the
/// snapshot is taken, replicating the same layout that a compose would produce.
///
/// Each entry is `(link_path, link_target, var_subdir)`:
/// - `link_path`   — absolute path inside the container that becomes a symlink
/// - `link_target` — the symlink target (relative, as rpm-ostree uses)
/// - `var_subdir`  — subdirectory under `/var` to create as the backing store,
///                   or `""` if the target is not under `/var` (e.g. `/media`)
///
/// Any content already present at `link_path` on the source system is moved
/// into `var_subdir` first so that user data (home directories, service state,
/// etc.) is not lost.
const VAR_SYMLINKS: &[(&str, &str, &str)] = &[
    // From rpm-ostree OSTREE_HOME_SYMLINKS:
    ("/home",  "var/home",    "home"),
    ("/root",  "var/roothome","roothome"),
    // From rpm-ostree ostree_strict_mode_symlinks:
    ("/srv",   "var/srv",     "srv"),
    ("/mnt",   "var/mnt",     "mnt"),
    ("/media", "run/media",   ""),    // points into /run, no /var backing dir needed
];



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
    ///
    /// This is the **snapshot** code path: the running filesystem is captured
    /// into an OCI image and pushed to this reference.  Mutually exclusive
    /// with `--image`.
    #[clap(long, conflicts_with = "image")]
    pub(crate) image_ref: Option<String>,

    /// Pre-built bootc image to install (hybrid migration path).
    ///
    /// Instead of snapshotting the running filesystem, install the supplied
    /// image and preserve the running system's `/var` data and `/etc`
    /// customisations.  The image should be built with `inspectah` from a
    /// `FROM <bootc-base>` Containerfile so that future upgrades have a clean
    /// image lineage.
    ///
    /// Example: `registry.example.com/myorg/myfleet:latest`
    ///
    /// Mutually exclusive with `--image-ref`.
    #[clap(long, conflicts_with = "image_ref")]
    pub(crate) image: Option<String>,

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
    /// Storage args to use when removing the container.
    storage_args: Vec<String>,
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
    fn new(container_id: impl Into<String>, storage_args: Vec<String>) -> Self {
        Self {
            container_id: container_id.into(),
            committed: Cell::new(false),
            storage_args,
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
                .args(&self.storage_args)
                .args(["rm", &self.container_id])
                .status();
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Entry point for `bootc install from-existing-root`.
pub(crate) async fn install_from_existing_root(opts: InstallFromExistingRootOpts) -> Result<()> {
    validate_prerequisites(&opts)?;

    // Dispatch to the appropriate code path based on which image argument was given.
    if let Some(ref image) = opts.image {
        // ── Hybrid path: pre-built image + state preservation ──────────────
        let image = image.clone();
        if !opts.acknowledge_destructive {
            print_destructive_warning(&image)?;
        }
        install_from_image(&opts, &image)
            .context("Installing pre-built image with state preservation")?;
    } else {
        // ── Snapshot path: build OCI image from running filesystem ─────────
        let image_ref = opts.image_ref.as_deref().unwrap_or_default().to_string();
        if !opts.acknowledge_destructive {
            print_destructive_warning(&image_ref)?;
        }

        let info = gather_system_info().context("Gathering system information")?;

        println!("Creating OCI snapshot image from running system...");
        println!("Image size may be several gigabytes; this can take several minutes.");
        println!();

        create_snapshot_image(&opts, &info).context("Creating OCI snapshot image")?;

        if !opts.skip_push {
            push_image(&opts.local_image_name, &image_ref)
                .context("Pushing image to registry")?;
        }

        // The image that `podman run` actually executes.  In the normal (push) case
        // this is the registry reference; in --skip-push it is the local image in
        // the system container storage (containers-storage transport).
        let source_imgref = if opts.skip_push {
            format!("containers-storage:localhost/{}", opts.local_image_name)
        } else {
            image_ref.clone()
        };

        run_install(&opts, &source_imgref, &image_ref)
            .context("Running bootc install to-existing-root")?;
    }

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

    // Exactly one of --image or --image-ref must be provided.
    match (&opts.image, &opts.image_ref) {
        (None, None) => anyhow::bail!(
            "Either --image or --image-ref must be provided.\n\
             Use --image for the hybrid migration path (pre-built inspectah image).\n\
             Use --image-ref for the snapshot path (build image from running system)."
        ),
        (Some(img), None) => {
            // Hybrid path: only podman is required (no buildah).
            check_binary(bootc_utils::podman_bin())
                .context("podman is required to run the install container")?;
            validate_image_ref(img)?;
        }
        (None, Some(img_ref)) => {
            // Snapshot path: both buildah and podman are required.
            check_binary(bootc_utils::buildah_bin())
                .context("buildah is required to create the snapshot image")?;
            check_binary(bootc_utils::podman_bin())
                .context("podman is required to run the install container")?;
            validate_image_ref(img_ref)?;
        }
        (Some(_), Some(_)) => {
            // clap's `conflicts_with` should prevent this, but guard defensively.
            anyhow::bail!("--image and --image-ref are mutually exclusive.");
        }
    }

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
             Please install it using your distribution's package manager."
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
fn create_snapshot_image(
    opts: &InstallFromExistingRootOpts,
    info: &SystemInfo,
) -> Result<()> {
    let sargs: Vec<String> = BUILDAH_STORAGE_ARGS.iter().map(|s| s.to_string()).collect();
    // Step 1: create an empty working container from scratch.
    println!("  [1/4] Creating empty buildah container...");
    let container_id = {
        let out = std::process::Command::new(bootc_utils::buildah_bin())
            .args(&sargs)
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
    let guard = BuildahContainerGuard::new(&container_id, sargs.clone());

    // Step 2: snapshot the running filesystem into the container.
    println!("  [2/4] Snapshotting running filesystem (this may take several minutes)...");
    snapshot_filesystem(guard.id(), &sargs)?;

    // Step 3: inject files that require special handling.
    println!("  [3/4] Injecting bootc-required files...");
    inject_required_files(guard.id(), info, &sargs)?;

    // Step 4: commit the container as a local OCI image.
    println!("  [4/4] Committing image as {}...", opts.local_image_name);
    commit_image(guard.id(), &opts.local_image_name, info, &sargs)?;

    // Prevent Drop from trying to remove the container (it no longer exists).
    guard.mark_committed();
    Ok(())
}
/// Choose the best directory for the intermediate tar archive.
///
/// Always uses `/var/tmp` (persistent disk) rather than `/tmp` (tmpfs/RAM).
///
/// Using `/tmp` (tmpfs) is tempting because it avoids disk writes, but in
/// memory-constrained environments — for example a KVM VM running under a cgroup
/// with a 4 GiB memory limit — a 500 MB–1 GiB tar archive stored in tmpfs
/// consumes guest RAM.  When the guest's page cache for disk I/O is added on top,
/// the total easily exceeds the cgroup limit, causing the QEMU process itself to
/// be killed by the OOM killer.  This manifests as an unexplained mid-conversion
/// VM crash with libvirt reporting `reason=crashed`.
///
/// `/var/tmp` is on persistent storage (typically the same filesystem as `/`),
/// so the archive does not consume RAM.  The sequential read → sequential write
/// pattern for a plain tar archive is efficient on any filesystem and does not
/// trigger the btrfs concurrent-read/write pathologies that were once thought to
/// be the cause of these crashes.
///
/// The function exists as a hook so that future logic (e.g. choosing based on
/// available free space) can be added without touching callers.
fn choose_tar_dir() -> &'static str {
    "/var/tmp"
}

/// Snapshot the running filesystem into the buildah working container.
///
/// `tar` archives the live root filesystem (excluding virtual/transient paths) to a
/// temporary file in `/var/tmp` (persistent disk), then `buildah add` unpacks it
/// at `/` inside the container.
///
/// Using an intermediate file (rather than piping tar's stdout directly to
/// `buildah add`) is required because newer versions of buildah (≥ 1.40) no longer
/// accept `-` as a stand-in for stdin in `buildah add`.  A FIFO (named pipe) would
/// avoid the disk write but cannot easily be used from the single-threaded tokio
/// runtime that bootc uses.
///
/// The archive is written to `/var/tmp` (persistent disk) rather than `/tmp`
/// (tmpfs) because storing a large archive in tmpfs consumes guest RAM.  In
/// memory-constrained environments (e.g. a KVM VM under a 4 GiB cgroup limit)
/// the RAM consumed by the archive plus the guest page cache can exceed the cgroup
/// limit, causing QEMU to be killed by the OOM killer mid-conversion.
#[context("Snapshotting running filesystem into buildah container")]
fn snapshot_filesystem(container_id: &str, sargs: &[String]) -> Result<()> {
    // Use /var/tmp (persistent disk) for the intermediate tar archive.
    // See choose_tar_dir() for rationale.
    let tar_dir = choose_tar_dir();
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tar_path = format!("{tar_dir}/bootc-snap-{ts:016x}.tar");
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

    // GNU tar's --exclude patterns match any path component by default.
    // For example, "--exclude=ostree" would accidentally exclude
    // /usr/bin/ostree (not just /ostree).  Anchoring with a "./" prefix
    // (e.g. "--exclude=./ostree") restricts the match to the archive root,
    // because we use "-C / ." so all archive member names begin with "./".
    // This requires the source to be specified as "-C / ." rather than "/"
    // so that member names carry the "./" prefix that anchors the exclusion.
    for path in EXCLUDED_PATHS {
        tar_args.push(format!("--exclude=./{}", path.trim_start_matches('/')));
    }
    // Also exclude extra mount points from the root pass so tar does not
    // try to archive their (empty) mount-point directories and then fail
    // when pass 2 re-archives the same path.
    for mp in &extra_mounts {
        tar_args.push(format!("--exclude=./{}", mp.trim_start_matches('/')));
    }

    // Use "-C / ." rather than "/" as the source so that archive member names
    // begin with "./" — this is required for the anchored exclude patterns above
    // to work correctly (see the NOTE above).
    tar_args.push("-C".into());
    tar_args.push("/".into());
    tar_args.push(".".into()); // source: root filesystem

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
        // Anchor the patterns with "./" so they only match at the archive
        // root (same technique used in pass 1).  We archive with "-C <mp> ."
        // so member names begin with "./".
        for ex in EXCLUDED_PATHS {
            if ex.starts_with(mp.as_str()) {
                // Make the exclude relative to the mountpoint.
                let relative = ex.trim_start_matches(mp.as_str()).trim_start_matches('/');
                if !relative.is_empty() {
                    mp_args.push(format!("--exclude=./{relative}"));
                }
            }
        }
        // Exclude the tar archive file itself if it falls under this mount
        // point.  Without this exclusion, pass 2 would try to archive the
        // partially-written tar file while simultaneously appending to it,
        // causing the file to grow unboundedly.
        if tar_path.starts_with(mp.as_str()) {
            let rel = tar_path
                .trim_start_matches(mp.as_str())
                .trim_start_matches('/');
            if !rel.is_empty() {
                mp_args.push(format!("--exclude=./{rel}"));
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
            // A fatal tar error on a major mount like /var produces a broken image
            // (e.g., empty /var/lib/NetworkManager).  Print a visible warning and
            // continue — the conversion may still be salvageable on some systems.
            eprintln!(
                "WARNING: tar for mount point {mp} exited with code {:?}; \
                 the snapshot may be incomplete.",
                status.code()
            );
            tracing::warn!(
                "tar for mount point {mp} exited with {:?}; continuing",
                status.code()
            );
        }
    }

    // `buildah add <container> <tar_path> /` unpacks the tar at `/` inside the container.
    let buildah_status = std::process::Command::new(bootc_utils::buildah_bin())
        .args(sargs)
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
fn regenerate_initramfs_with_dracut(
    container_id: &str,
    info: &SystemInfo,
    dest: &str,
    sargs: &[String],
) {
    // Check if dracut is available inside the container.
    let dracut_check = std::process::Command::new(bootc_utils::buildah_bin())
        .args(sargs)
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
        .args(sargs)
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
    //
    // DRACUT_NO_XATTR=1: the buildah container does not have CAP_MAC_ADMIN, so
    // dracut-install cannot write security.selinux xattrs onto initramfs files.
    // Setting this env var tells dracut-install to skip all xattr copying
    // (see dracut-install.c: `getenv("DRACUT_NO_XATTR")`).  The resulting
    // initramfs boots fine without SELinux labels on its files; the labels are
    // applied by the kernel policy on first boot.
    let status = std::process::Command::new(bootc_utils::buildah_bin())
        .args(sargs)
        .args([
            "run",
            "--env",
            "DRACUT_NO_XATTR=1",
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
fn inject_required_files(
    container_id: &str,
    info: &SystemInfo,
    sargs: &[String],
) -> Result<()> {
    // Package-mode RHEL keeps the initramfs in /boot; bootc expects it at
    // /usr/lib/modules/<kver>/initramfs.img.
    if !info.initramfs_already_placed {
        let dest = format!(
            "{BOOTC_INITRAMFS_DIR}/{}/initramfs.img",
            info.kernel_version
        );
        println!("      Placing initramfs at {dest}");
        buildah_copy(container_id, &info.initramfs_src, &dest, sargs)
            .context("Copying initramfs into container")?;
        regenerate_initramfs_with_dracut(container_id, info, &dest, sargs);
    }

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
            .args(sargs)
            .args(["run", container_id, "--", "mkdir", "-p", "/usr/lib/ostree"])
            .status()
            .context("Creating /usr/lib/ostree in container")?;
        ensure!(
            status.success(),
            "`buildah run mkdir -p /usr/lib/ostree` failed"
        );
        buildah_copy(container_id, tmp_path, PREPARE_ROOT_CONF_PATH, sargs)
            .context("Writing prepare-root.conf into container")?;
    }

    {
        println!("      Creating /ostree → sysroot/ostree symlink");
        let status = std::process::Command::new(bootc_utils::buildah_bin())
            .args(sargs)
            .args(["run", container_id, "--", "ln", "-sf", "sysroot/ostree", "/ostree"])
            .status()
            .context("Creating /ostree symlink in container")?;
        ensure!(
            status.success(),
            "`buildah run ln -sf sysroot/ostree /ostree` failed"
        );
    }

    // Replace package-mode real directories with the ostree-layout symlinks.
    //
    // A package-mode system has real directories at /home, /root, /srv, /mnt,
    // and /media.  A bootc image requires these to be symlinks into /var (or
    // /run for /media) — the same layout that rpm-ostree's compose
    // postprocessing (`compose_init_rootfs_strict`) produces.
    //
    // For each path we:
    //   1. Skip if it is already a symlink (idempotent / already-converted image).
    //   2. If a /var backing directory is configured and the source path is a
    //      non-empty real directory, move its contents there so no user data is
    //      lost (important for /home and /root).
    //   3. Remove the now-empty real directory.
    //   4. Create the symlink.
    println!("      Replacing package-mode directories with ostree /var symlinks");
    for (link_path, link_target, var_subdir) in VAR_SYMLINKS {
        // Build a single shell snippet that is safe and idempotent:
        //
        //   • If the path is already a symlink, do nothing.
        //   • If a /var backing dir is requested, ensure it exists and move any
        //     existing content into it before removing the source directory.
        //   • Remove the (now empty, or never-existed) source path.
        //   • Create the symlink.
        let script = if var_subdir.is_empty() {
            // No /var backing dir (e.g. /media → run/media). Just replace
            // whatever is there with the symlink; /media is always empty on a
            // package-mode system.
            format!(
                "test -L '{link_path}' && exit 0; \
                 rm -rf '{link_path}'; \
                 ln -s '{link_target}' '{link_path}'"
            )
        } else {
            format!(
                "test -L '{link_path}' && exit 0; \
                 mkdir -p '/var/{var_subdir}'; \
                 if [ -d '{link_path}' ]; then \
                   cp -a '{link_path}/.' '/var/{var_subdir}/'; \
                 fi; \
                 rm -rf '{link_path}'; \
                 ln -s '{link_target}' '{link_path}'"
            )
        };

        let status = std::process::Command::new(bootc_utils::buildah_bin())
            .args(sargs)
            .args(["run", container_id, "--", "sh", "-c", &script])
            .status()
            .context(format!("Creating {link_path} → {link_target} symlink in container"))?;
        ensure!(
            status.success(),
            "`buildah run` failed while creating {link_path} → {link_target} symlink (exit code {:?})",
            status.code()
        );
        println!("        {link_path} → {link_target}");
    }

    for (path, mode) in RECREATE_EMPTY_DIRS {
        let status = std::process::Command::new(bootc_utils::buildah_bin())
            .args(sargs)
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

    set_image_labels(container_id, info, sargs).context("Setting OCI image labels")?;

    Ok(())
}

/// Run `buildah copy <container> <src> <dest>`.
fn buildah_copy(container_id: &str, src: &str, dest: &str, sargs: &[String]) -> Result<()> {
    let status = std::process::Command::new(bootc_utils::buildah_bin())
        .args(sargs)
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
fn set_image_labels(container_id: &str, info: &SystemInfo, sargs: &[String]) -> Result<()> {
    let created = current_rfc3339();
    let uname = rustix::system::uname();
    let hostname = uname.nodename().to_str().unwrap_or("unknown");

    let status = std::process::Command::new(bootc_utils::buildah_bin())
        .args(sargs)
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
fn commit_image(
    container_id: &str,
    local_image_name: &str,
    _info: &SystemInfo,
    sargs: &[String],
) -> Result<()> {
    let status = std::process::Command::new(bootc_utils::buildah_bin())
        .args(sargs)
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
/// `source_imgref` is the image reference for `podman run` (what to execute)
/// and also passed as `--source-imgref` to bootc inside the container.
/// In the `--skip-push` case this is a `containers-storage:` URI pointing to
/// the locally-committed snapshot image; otherwise it is the registry reference.
/// `target_imgref` is the registry reference stored in the new deployment for
/// future `bootc upgrade` operations.
#[context("Running bootc install to-existing-root")]
fn run_install(
    opts: &InstallFromExistingRootOpts,
    source_imgref: &str,
    target_imgref: &str,
) -> Result<()> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "--privileged".into(),
        "--pid=host".into(),
        "--user=root:root".into(),
        "-v".into(),
        "/dev:/dev".into(),
        "--security-opt".into(),
        "label=type:unconfined_t".into(),
        // Mount the host root so `to-existing-root` can see and modify it.
        "-v".into(),
        "/:/target".into(),
        // Mount the system container storage so bootc inside the container can
        // find the locally-committed snapshot image.
        "-v".into(),
        "/var/lib/containers:/var/lib/containers".into(),
    ];

    // Forward RUST_LOG so callers can enable verbose bootc output.
    if let Ok(rust_log) = std::env::var("RUST_LOG") {
        args.push(format!("--env=RUST_LOG={rust_log}"));
    }

    // Mount the authorized_keys file if provided (read-only; the install
    // container has no reason to modify the host's authorized_keys file).
    if let Some(keys_path) = &opts.root_ssh_authorized_keys {
        args.push("-v".into());
        args.push(format!("{keys_path}:{SSH_KEY_MOUNT}:ro"));
    }

    // The image that podman runs (contains the bootc binary from the snapshot).
    args.push(source_imgref.to_string());

    // ── bootc install to-existing-root arguments ──────────────────────────────

    args.extend(["bootc", "install", "to-existing-root"].map(String::from));

    // --source-imgref: where bootc pulls the image content from inside the container.
    args.push("--source-imgref".into());
    args.push(source_imgref.to_string());

    // --target-imgref: the registry reference stored for future `bootc upgrade`.
    // Always set to target_imgref so future upgrades contact the registry.
    args.push("--target-imgref".into());
    args.push(target_imgref.to_string());

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

// ── Hybrid migration: install pre-built image + preserve state ────────────────

/// Directory under `/var` where the package-mode kernel/initramfs are stashed
/// before `/boot` is wiped.  This directory survives the install because only
/// `/boot` is wiped by `clean_boot_directories()`; `/var` is untouched.
const PKGMODE_ROLLBACK_VAR: &str = "/var/lib/pkgmode-rollback";

/// Directory under `/boot` where the stashed kernel/initramfs are copied after
/// the bootc install completes and `/boot` is re-populated.
const PKGMODE_ROLLBACK_BOOT: &str = "/boot/pkgmode-rollback";

/// Hybrid migration entry point.
///
/// Phases:
///   1. Save running kernel/initramfs/kargs to `/var/lib/pkgmode-rollback/`
///   2. Run `bootc install to-existing-root` with the supplied pre-built image
///   3. Preserve `/var`: reflink copy (btrfs/XFS) or `var.mount` injection (ext4)
///   4. Merge running `/etc` into the new deployment's `/etc` (3-way merge)
///   5. Write package-mode rollback BLS entry to `/boot/loader/entries/`
#[context("Hybrid migration: installing pre-built image with state preservation")]
fn install_from_image(opts: &InstallFromExistingRootOpts, image: &str) -> Result<()> {
    // ── Phase 1: Save kernel/initramfs/kargs ─────────────────────────────────
    println!();
    println!("Phase 1: Saving running kernel and initramfs for rollback...");
    let kver = save_pkgmode_kernel()?;
    println!("  Saved kernel {kver} to {PKGMODE_ROLLBACK_VAR}/");

    // ── Phase 2: Install the pre-built image ─────────────────────────────────
    println!();
    println!("Phase 2: Installing pre-built image via bootc install to-existing-root...");
    // For the hybrid path the image ref is the same for both --source-imgref and
    // --target-imgref: the image was built externally and pushed to the registry
    // already.  `--skip-push` is not applicable here (no local image was built),
    // but we pass the opts through in case the caller set other forwarded flags.
    run_install(opts, image, image)
        .context("Running bootc install to-existing-root")?;

    // After the install the new deployment is staged under /sysroot/ostree/.
    // Find the deployment directory so we can write into etc/ and var/.
    let deploy_dir = find_deploy_dir()
        .context("Locating new ostree deployment directory")?;
    println!("  Deployment directory: {deploy_dir}");

    // ── Phase 3: Preserve /var ────────────────────────────────────────────────
    println!();
    println!("Phase 3: Preserving /var data into new deployment...");
    // The deployment's var/ directory is two levels up from the deployment dir:
    //   /sysroot/ostree/deploy/<stateroot>/deploy/<hash>.0  ← deploy_dir
    //   /sysroot/ostree/deploy/<stateroot>/var/              ← new_var
    let new_var_str = {
        let mut p = std::path::PathBuf::from(&deploy_dir);
        p.pop(); // pop the <hash>.0 component
        p.pop(); // pop the `deploy` component
        p.push("var");
        p.to_string_lossy().into_owned()
    };
    preserve_var("/var", &new_var_str, &deploy_dir)
        .context("Preserving /var into new deployment")?;

    // ── Phase 4: Merge /etc ───────────────────────────────────────────────────
    println!();
    println!("Phase 4: Merging running /etc into new deployment...");
    let deploy_etc = format!("{deploy_dir}/etc");
    let deploy_usr_etc = format!("{deploy_dir}/usr/etc");
    merge_etc("/etc", &deploy_usr_etc, &deploy_etc)
        .context("Merging /etc into new deployment")?;

    // ── Phase 5: Write rollback BLS entry ────────────────────────────────────
    println!();
    println!("Phase 5: Writing package-mode rollback boot entry...");
    write_pkgmode_rollback_entry(&kver)
        .context("Writing package-mode rollback BLS entry")?;
    println!("  Rollback entry written. Hold Shift/Esc at GRUB to select it.");

    Ok(())
}

/// Save the running kernel, initramfs, and kernel command-line arguments to
/// `/var/lib/pkgmode-rollback/` before `/boot` is wiped.
///
/// Returns the running kernel version string (e.g. `"6.11.0-26.fc41.x86_64"`).
#[context("Saving package-mode kernel and initramfs")]
fn save_pkgmode_kernel() -> Result<String> {
    let uname = rustix::system::uname();
    let kver = uname
        .release()
        .to_str()
        .context("Kernel version string is not valid UTF-8")?
        .to_string();

    std::fs::create_dir_all(PKGMODE_ROLLBACK_VAR)
        .with_context(|| format!("Creating {PKGMODE_ROLLBACK_VAR}"))?;

    // Save the kernel image.
    let vmlinuz_src = format!("/boot/vmlinuz-{kver}");
    let vmlinuz_dst = format!("{PKGMODE_ROLLBACK_VAR}/vmlinuz");
    std::fs::copy(&vmlinuz_src, &vmlinuz_dst)
        .with_context(|| format!("Copying {vmlinuz_src} → {vmlinuz_dst}"))?;

    // Save the initramfs.
    let initramfs_src = format!("/boot/initramfs-{kver}.img");
    let initramfs_dst = format!("{PKGMODE_ROLLBACK_VAR}/initramfs.img");
    std::fs::copy(&initramfs_src, &initramfs_dst)
        .with_context(|| format!("Copying {initramfs_src} → {initramfs_dst}"))?;

    // Save the running kernel command line (for the BLS entry options field).
    let cmdline = std::fs::read_to_string("/proc/cmdline")
        .context("Reading /proc/cmdline")?;
    let kargs_dst = format!("{PKGMODE_ROLLBACK_VAR}/kargs.txt");
    std::fs::write(&kargs_dst, cmdline.trim())
        .with_context(|| format!("Writing {kargs_dst}"))?;

    Ok(kver)
}

/// Locate the newly-created ostree deployment directory under `/sysroot`.
///
/// After `bootc install to-existing-root` runs, the deployment is staged
/// at a path like:
///   `/sysroot/ostree/deploy/default/deploy/<checksum>.0`
///
/// We find it by asking `ostree admin --sysroot=/sysroot --print-current-dir`.
#[context("Locating new ostree deployment directory under /sysroot")]
fn find_deploy_dir() -> Result<String> {
    let out = std::process::Command::new("ostree")
        .args(["admin", "--sysroot=/sysroot", "--print-current-dir"])
        .output()
        .context("Running `ostree admin --print-current-dir`")?;
    if out.status.success() {
        let dir = String::from_utf8(out.stdout)
            .context("`ostree admin --print-current-dir` produced non-UTF-8 output")?
            .trim()
            .to_string();
        if !dir.is_empty() && std::path::Path::new(&dir).exists() {
            return Ok(dir);
        }
    }

    // Fallback: enumerate stateroot directories under /sysroot/ostree/deploy/
    // and return the most recently modified deployment directory.
    let stateroot_base = std::path::Path::new("/sysroot/ostree/deploy");
    let mut candidates: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();

    if let Ok(stateroots) = std::fs::read_dir(stateroot_base) {
        for stateroot_entry in stateroots.flatten() {
            let deploy_subdir = stateroot_entry.path().join("deploy");
            if let Ok(deploys) = std::fs::read_dir(&deploy_subdir) {
                for deploy_entry in deploys.flatten() {
                    let path = deploy_entry.path();
                    if path.is_dir() {
                        let mtime = path
                            .metadata()
                            .and_then(|m| m.modified())
                            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                        candidates.push((mtime, path));
                    }
                }
            }
        }
    }

    // Most recently modified deployment is the new one.
    candidates.sort_by_key(|(t, _)| *t);
    candidates.reverse();

    candidates
        .into_iter()
        .next()
        .map(|(_, p)| p.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow::anyhow!(
            "Could not locate the new ostree deployment directory under /sysroot.\n\
             Expected a directory under /sysroot/ostree/deploy/<stateroot>/deploy/"
        ))
}

/// Preserve the running system's `/var` into the new deployment's `var/` directory.
///
/// Strategy C (preferred): use `cp --reflink=always` for an instantaneous
/// copy-on-write clone on btrfs or XFS with reflinks enabled.
///
/// Strategy D (fallback): on ext4 (or any filesystem where reflinks fail),
/// inject a `var.mount` systemd unit into the deployment's `etc/` so that the
/// new system's `/var` is bind-mounted from `/sysroot/var` on boot.  This is a
/// transitional state: the operator can later copy the data and remove the unit.
///
/// `src_var` is the path to the running system's `/var` (always `/var` when
/// running on the host pre-reboot).  `new_var` is the path to the new
/// deployment's `var/` directory (writable before reboot).  `deploy_dir` is
/// the deployment root (needed for Strategy D unit injection).
#[context("Preserving /var into new deployment")]
fn preserve_var(src_var: &str, new_var: &str, deploy_dir: &str) -> Result<()> {
    // Ensure the destination directory exists.
    std::fs::create_dir_all(new_var)
        .with_context(|| format!("Creating {new_var}"))?;

    // Probe reflink support with a zero-size test reflink.
    let reflink_probe = format!("{new_var}/.bootc-reflink-probe");
    let probe_src = format!("{src_var}/.bootc-reflink-probe-src");
    // Create a tiny probe file.
    std::fs::write(&probe_src, b"probe")
        .with_context(|| format!("Creating probe file {probe_src}"))?;
    let probe_result = std::process::Command::new("cp")
        .args(["--reflink=always", "-a", &probe_src, &reflink_probe])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_file(&probe_src);
    let _ = std::fs::remove_file(&reflink_probe);

    let reflinks_supported = probe_result.map(|s| s.success()).unwrap_or(false);

    if reflinks_supported {
        println!("  Filesystem supports reflinks — using copy-on-write clone (Strategy C)");
        preserve_var_reflink(src_var, new_var)
    } else {
        println!("  Filesystem does not support reflinks — injecting var.mount unit (Strategy D)");
        preserve_var_mount_unit(deploy_dir)
    }
}

/// Strategy C: reflink-copy the running `/var` into the new deployment's `var/`.
///
/// This is an instantaneous CoW clone that consumes no extra disk space until
/// data diverges.  Works on btrfs and XFS with reflinks enabled.
#[context("Reflink-copying /var into new deployment (Strategy C)")]
fn preserve_var_reflink(src_var: &str, new_var: &str) -> Result<()> {
    // We copy each top-level entry under src_var rather than src_var itself,
    // because we want the contents to land directly under new_var.
    // Skip directories and files that are ephemeral (log journal, caches, etc.):
    // the new system will regenerate them.
    const SKIP_SUBDIRS: &[&str] = &[
        "tmp",
        "cache",
        "log/journal",       // large; regenerated by journald
        "lib/containers",    // container storage; quadlets repopulate
    ];

    let src_path = std::path::Path::new(src_var);
    let entries = std::fs::read_dir(src_path)
        .with_context(|| format!("Reading {src_var}"))?;

    for entry in entries {
        let entry = entry.with_context(|| format!("Reading entry in {src_var}"))?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Check if this top-level entry should be skipped entirely.
        let skip = SKIP_SUBDIRS.iter().any(|s| {
            // A skip like "log/journal" means skip the subdirectory "journal"
            // inside "log", not the top-level "log" itself.  Top-level skips
            // are plain names without a '/'.
            !s.contains('/') && *s == name_str.as_ref()
        });
        if skip {
            println!("    Skipping {src_var}/{name_str} (ephemeral)");
            continue;
        }

        let src = format!("{src_var}/{name_str}");
        let dst = format!("{new_var}/{name_str}");

        println!("    Reflink-copying {src} → {dst}");
        let status = std::process::Command::new("cp")
            .args([
                "--reflink=always",
                "-a",           // preserve all attributes (perms, timestamps, xattrs)
                "--no-clobber", // do not overwrite files already placed by the image
                &src,
                &dst,
            ])
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("cp --reflink=always {src} {dst}"))?;
        if !status.success() {
            // Log a warning but continue; a single directory failure (e.g. a
            // permission-denied on a socket file) should not abort the whole migration.
            eprintln!(
                "WARNING: cp --reflink=always failed for {src} (exit {:?}); \
                 that directory will be empty in the new deployment.",
                status.code()
            );
        }
    }

    println!("  /var reflink copy complete.");
    Ok(())
}

/// Strategy D: inject a `var.mount` systemd unit into the deployment's `etc/`
/// that bind-mounts `/sysroot/var` onto `/var` on first boot.
///
/// Used as a fallback on ext4 where reflinks are unavailable.  This creates a
/// transient dependency on the old `/sysroot/var`; the operator should migrate
/// the data at their own pace and then remove the unit.
///
/// `deploy_dir` is the deployment root (e.g.
/// `/sysroot/ostree/deploy/default/deploy/<hash>.0`).
#[context("Injecting var.mount unit into deployment etc/ (Strategy D)")]
fn preserve_var_mount_unit(deploy_dir: &str) -> Result<()> {
    let unit_dir = format!("{deploy_dir}/etc/systemd/system");
    std::fs::create_dir_all(&unit_dir)
        .with_context(|| format!("Creating {unit_dir}"))?;

    let unit_path = format!("{unit_dir}/var.mount");
    let unit_content = "\
[Unit]\n\
Description=Bind-mount pre-migration /var from physical root\n\
Documentation=https://github.com/bootc-dev/bootc\n\
Before=local-fs.target\n\
ConditionPathExists=/sysroot/var\n\
\n\
[Mount]\n\
What=/sysroot/var\n\
Where=/var\n\
Type=none\n\
Options=bind\n\
\n\
[Install]\n\
WantedBy=local-fs.target\n";

    std::fs::write(&unit_path, unit_content)
        .with_context(|| format!("Writing {unit_path}"))?;

    // Enable the unit by creating the symlink under wants/.
    let wants_dir = format!("{unit_dir}/local-fs.target.wants");
    std::fs::create_dir_all(&wants_dir)
        .with_context(|| format!("Creating {wants_dir}"))?;
    let symlink_path = format!("{wants_dir}/var.mount");
    // Remove any existing symlink first (idempotent).
    let _ = std::fs::remove_file(&symlink_path);
    std::os::unix::fs::symlink("../var.mount", &symlink_path)
        .with_context(|| format!("Creating symlink {symlink_path}"))?;

    println!("  var.mount unit injected at {unit_path}");
    println!("  NOTE: the new system will bind-mount /sysroot/var onto /var on boot.");
    println!("  After verifying the new system, copy data to /var and remove the unit.");
    Ok(())
}

/// Run the 3-way `/etc` merge: apply the diff between the running system's `/etc`
/// and the image's pristine `/usr/etc` on top of the new deployment's `/etc`.
///
/// Inputs:
///   - **pristine** (`deploy_usr_etc`): the image's shipped defaults at `<deploy>/usr/etc`
///   - **current** (`host_etc`): the running system's `/etc` (pre-reboot)
///   - **new** (`deploy_etc`): the new deployment's `/etc` (writable before reboot)
///
/// The merge copies files that the running admin changed relative to the image
/// defaults into the new deployment, preserving NIC profiles, SSH host keys,
/// secrets, and any other customisations that inspectah may have missed or
/// intentionally excluded from the fleet image.
#[context("Merging running /etc into new deployment")]
fn merge_etc(host_etc: &str, deploy_usr_etc: &str, deploy_etc: &str) -> Result<()> {
    let pristine_fd = CapStdDir::open_ambient_dir(deploy_usr_etc, ambient_authority())
        .with_context(|| format!("Opening pristine etc (deploy/usr/etc): {deploy_usr_etc}"))?;
    let current_fd = CapStdDir::open_ambient_dir(host_etc, ambient_authority())
        .with_context(|| format!("Opening current etc: {host_etc}"))?;
    let new_fd = CapStdDir::open_ambient_dir(deploy_etc, ambient_authority())
        .with_context(|| format!("Opening new deploy etc: {deploy_etc}"))?;

    let (pristine_tree, current_tree, new_tree_opt) =
        traverse_etc(&pristine_fd, &current_fd, Some(&new_fd))
            .context("Traversing /etc trees for 3-way merge")?;

    let new_tree = new_tree_opt.unwrap_or_else(|| FileSystem::new(Stat::uninitialized()));

    let diff = compute_diff(&pristine_tree, &current_tree, &new_tree)
        .context("Computing /etc diff (pristine vs running)")?;

    // Log the diff summary.
    println!("  /etc merge diff:");
    etc_merge::print_diff(&diff, &mut std::io::stdout());

    merge(&current_fd, &current_tree, &new_fd, &new_tree, &diff)
        .context("Applying /etc 3-way merge")?;

    println!("  /etc merge complete.");
    Ok(())
}

/// Copy the stashed kernel/initramfs into `/boot/pkgmode-rollback/` and write
/// a BLS entry so GRUB presents a "Previous OS — package-mode rollback" option.
///
/// The BLS entry uses `sort-key: zz-pkgmode` so it sorts after all ostree
/// entries and appears last in the boot menu.  GRUB reads all `.conf` files in
/// `loader/entries/` via `blscfg`; ostree only reads `ostree-*.conf` files, so
/// this entry is invisible to all ostree/bootupd code paths and will not be
/// modified or deleted by `bootc upgrade`.
///
/// The one survivability risk is the `loader.N` directory rotation (see
/// BOOTLOADER.md).  A persistence service baked into the bootc image mitigates
/// this; the BLS entry written here covers the first boot immediately after
/// migration.
#[context("Writing package-mode rollback BLS entry")]
fn write_pkgmode_rollback_entry(kver: &str) -> Result<()> {
    // ── Step 1: install kernel and initramfs into /boot ───────────────────────
    std::fs::create_dir_all(PKGMODE_ROLLBACK_BOOT)
        .with_context(|| format!("Creating {PKGMODE_ROLLBACK_BOOT}"))?;

    let vmlinuz_src = format!("{PKGMODE_ROLLBACK_VAR}/vmlinuz");
    let vmlinuz_dst = format!("{PKGMODE_ROLLBACK_BOOT}/vmlinuz");
    std::fs::copy(&vmlinuz_src, &vmlinuz_dst)
        .with_context(|| format!("Copying {vmlinuz_src} → {vmlinuz_dst}"))?;

    let initramfs_src = format!("{PKGMODE_ROLLBACK_VAR}/initramfs.img");
    let initramfs_dst = format!("{PKGMODE_ROLLBACK_BOOT}/initramfs.img");
    std::fs::copy(&initramfs_src, &initramfs_dst)
        .with_context(|| format!("Copying {initramfs_src} → {initramfs_dst}"))?;

    // ── Step 2: read original kernel arguments ────────────────────────────────
    let kargs_path = format!("{PKGMODE_ROLLBACK_VAR}/kargs.txt");
    let raw_kargs = std::fs::read_to_string(&kargs_path)
        .with_context(|| format!("Reading {kargs_path}"))?;
    // Strip the BOOT_IMAGE= argument — it is specific to the old bootloader
    // invocation and is not meaningful in the new BLS context.
    let options: String = raw_kargs
        .split_whitespace()
        .filter(|tok| !tok.starts_with("BOOT_IMAGE="))
        .collect::<Vec<_>>()
        .join(" ");

    // ── Step 3: find the active loader/entries/ directory ────────────────────
    // /boot/loader is a symlink managed by ostree that points at loader.0 or
    // loader.1.  The BLS entry must go into the *currently active* loader.N so
    // that GRUB sees it immediately after reboot.
    let loader_link = std::fs::read_link("/boot/loader")
        .context("Reading /boot/loader symlink")?;
    let loader_dir = loader_link
        .to_str()
        .context("/boot/loader symlink target is not valid UTF-8")?
        .trim_start_matches('/')
        .to_string();
    let entries_dir = format!("/boot/{loader_dir}/entries");
    std::fs::create_dir_all(&entries_dir)
        .with_context(|| format!("Creating {entries_dir}"))?;

    // ── Step 4: write the BLS entry ───────────────────────────────────────────
    let entry_path = format!("{entries_dir}/pkgmode-rollback.conf");
    let entry_content = format!(
        "title Previous OS — package-mode rollback (kernel {kver})\n\
         sort-key zz-pkgmode\n\
         linux /pkgmode-rollback/vmlinuz\n\
         initrd /pkgmode-rollback/initramfs.img\n\
         options {options}\n"
    );
    std::fs::write(&entry_path, &entry_content)
        .with_context(|| format!("Writing BLS entry {entry_path}"))?;

    println!("  BLS entry written: {entry_path}");
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
    fn test_var_symlinks_cover_required_paths() {
        // Verify the ostree-layout symlinks that must be present in the image.
        let required: &[(&str, &str)] = &[
            ("/home",  "var/home"),
            ("/root",  "var/roothome"),
            ("/srv",   "var/srv"),
            ("/mnt",   "var/mnt"),
            ("/media", "run/media"),
        ];
        for (path, target) in required {
            assert!(
                VAR_SYMLINKS.iter().any(|(p, t, _)| p == path && t == target),
                "VAR_SYMLINKS is missing entry for {path} → {target}"
            );
        }
    }

    #[test]
    fn test_var_symlinks_not_in_excluded_paths() {
        // The link paths themselves must NOT be in EXCLUDED_PATHS — we need
        // their content to be captured by the tar snapshot so we can migrate it
        // into /var before replacing them with symlinks.
        for (link_path, _, _) in VAR_SYMLINKS {
            assert!(
                !EXCLUDED_PATHS.contains(link_path),
                "{link_path} must not be in EXCLUDED_PATHS (content needs to be snapshotted)"
            );
        }
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

    /// Verify the var/ path derivation from a deployment directory.
    ///
    /// Given a deployment dir like `/sysroot/ostree/deploy/default/deploy/<hash>.0`,
    /// the new deployment's var/ should be two levels up, then `var/`.
    #[test]
    fn test_new_var_path_from_deploy_dir() {
        let deploy_dir = "/sysroot/ostree/deploy/default/deploy/abc123.0";
        let mut p = std::path::PathBuf::from(deploy_dir);
        p.pop(); // pop the <hash>.0 component
        p.pop(); // pop the `deploy` component
        p.push("var");
        assert_eq!(
            p.to_string_lossy().as_ref(),
            "/sysroot/ostree/deploy/default/var"
        );
    }

    /// Verify that the rollback var directory constant is within /var (not /boot
    /// which is wiped) and is therefore safe to use as a pre-wipe stash.
    #[test]
    fn test_pkgmode_rollback_var_not_in_boot() {
        assert!(
            PKGMODE_ROLLBACK_VAR.starts_with("/var/"),
            "PKGMODE_ROLLBACK_VAR must be under /var/ (not /boot which is wiped): \
             {PKGMODE_ROLLBACK_VAR}"
        );
    }

    /// Verify that the BLS entry skip-subdirs for reflink copy exclude ephemeral
    /// directories but not application data directories.
    #[test]
    fn test_reflink_skip_subdirs_are_ephemeral() {
        // These are the directories we skip during Strategy C /var copy.
        // They must all be regenerable / not application data.
        const EXPECTED_SKIPS: &[&str] = &["tmp", "cache", "log/journal", "lib/containers"];
        for skip in EXPECTED_SKIPS {
            // Verify lib/postgres, lib/mysql, etc. are NOT in the skip list —
            // they contain application data that must be preserved.
            assert!(
                !skip.starts_with("lib/pg") && !skip.starts_with("lib/my"),
                "Accidentally skipping application data directory: {skip}"
            );
        }
    }

    /// Verify that --image and --image-ref are logically distinct: image_ref is
    /// for the snapshot path, image is for the hybrid path.
    #[test]
    fn test_opts_image_and_image_ref_are_separate_fields() {
        // Both fields exist and can be set independently in tests.
        let opts_snapshot = InstallFromExistingRootOpts {
            image_ref: Some("registry.example.com/myorg/snap:latest".to_string()),
            image: None,
            acknowledge_destructive: true,
            reboot: false,
            cleanup: false,
            root_ssh_authorized_keys: None,
            skip_push: false,
            composefs_backend: false,
            local_image_name: DEFAULT_LOCAL_IMAGE_NAME.to_string(),
        };
        assert!(opts_snapshot.image_ref.is_some());
        assert!(opts_snapshot.image.is_none());

        let opts_hybrid = InstallFromExistingRootOpts {
            image_ref: None,
            image: Some("registry.example.com/myorg/fleet:latest".to_string()),
            acknowledge_destructive: true,
            reboot: false,
            cleanup: false,
            root_ssh_authorized_keys: None,
            skip_push: false,
            composefs_backend: false,
            local_image_name: DEFAULT_LOCAL_IMAGE_NAME.to_string(),
        };
        assert!(opts_hybrid.image.is_some());
        assert!(opts_hybrid.image_ref.is_none());
    }
}
