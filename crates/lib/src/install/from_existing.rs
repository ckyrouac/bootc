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
//! below. The `--one-file-system` tar flag automatically omits separately-mounted
//! filesystems (e.g. a separately-mounted `/home` or `/var` partition), so content
//! on the same physical device as `/` is captured while separate mount points are not.
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
/// Written into the image only when the running system does not already have this file.
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
];

/// Default name given to the intermediate image in local container storage.
const DEFAULT_LOCAL_IMAGE_NAME: &str = "bootc-snapshot:latest";

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
    /// Whether `/usr/lib/ostree/prepare-root.conf` already exists
    has_prepare_root_conf: bool,
}

impl std::fmt::Debug for SystemInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemInfo")
            .field("kernel_version", &self.kernel_version)
            .field("initramfs_src", &self.initramfs_src)
            .field("initramfs_already_placed", &self.initramfs_already_placed)
            .field("has_prepare_root_conf", &self.has_prepare_root_conf)
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

    let has_prepare_root_conf = std::path::Path::new(PREPARE_ROOT_CONF_PATH).exists();
    if !has_prepare_root_conf {
        println!(
            "NOTE: {PREPARE_ROOT_CONF_PATH} not found on this system; \
             a minimal version will be injected into the image."
        );
    }

    Ok(SystemInfo {
        kernel_version,
        initramfs_src,
        initramfs_already_placed,
        has_prepare_root_conf,
    })
}

// ── Phase 3: Create OCI image ─────────────────────────────────────────────────

#[context("Creating OCI snapshot image")]
fn create_snapshot_image(opts: &InstallFromExistingRootOpts, info: &SystemInfo) -> Result<()> {
    // Step 1: create an empty working container from scratch.
    println!("  [1/4] Creating empty buildah container...");
    let container_id = {
        let out = std::process::Command::new(bootc_utils::buildah_bin())
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

/// Pipe the running filesystem through `tar` into `buildah add`, excluding
/// virtual/transient paths.  No temporary tar file is written to disk.
#[context("Snapshotting running filesystem into buildah container")]
fn snapshot_filesystem(container_id: &str) -> Result<()> {
    let mut tar_args: Vec<String> = vec![
        "--create".into(),
        "--file=-".into(), // write to stdout for piping
        // Do not cross mount-point boundaries.  This naturally excludes tmpfs
        // mounts (proc, sys, dev, run) and any separately-mounted partitions.
        "--one-file-system".into(),
        "--sparse".into(),
        "--acls".into(),
        // Preserve all extended attributes, including SELinux security contexts.
        "--xattrs".into(),
    ];

    for path in EXCLUDED_PATHS {
        tar_args.push(format!("--exclude={path}"));
    }

    tar_args.push("/".into()); // source: root filesystem

    // Spawn tar and pipe its stdout directly into `buildah add` stdin,
    // avoiding a large temporary file on disk.
    let mut tar_child = std::process::Command::new("tar")
        .args(&tar_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Spawning tar")?;

    let tar_stdout = tar_child
        .stdout
        .take()
        .expect("tar stdout must be piped");

    // `buildah add <container> - /` reads a tar from stdin and unpacks it
    // at `/` inside the container.
    let buildah_status = std::process::Command::new(bootc_utils::buildah_bin())
        .args(["add", "--quiet", container_id, "-", "/"])
        .stdin(Stdio::from(tar_stdout))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Running `buildah add`")?;

    let tar_status = tar_child.wait().context("Waiting for tar")?;

    // GNU tar exits with code 1 for non-fatal warnings ("file changed as we
    // read it") which are expected for a live snapshot; code 2 is fatal.
    let tar_ok = tar_status.code().map(|c| c <= 1).unwrap_or(false);
    ensure!(
        tar_ok,
        "tar exited with a fatal error (exit code {:?})",
        tar_status.code()
    );
    ensure!(
        buildah_status.success(),
        "`buildah add` failed (exit code {:?})",
        buildah_status.code()
    );

    Ok(())
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
    }

    // /usr/lib/ostree/prepare-root.conf is mandatory for ostree to configure
    // the mount namespace at boot time.
    if !info.has_prepare_root_conf {
        let tmp =
            tempfile::NamedTempFile::new().context("Creating tempfile for prepare-root.conf")?;
        tmp.as_file()
            .write_all(PREPARE_ROOT_CONF_CONTENT)
            .context("Writing prepare-root.conf content")?;
        let tmp_path = tmp
            .path()
            .to_str()
            .context("Tempfile path is not valid UTF-8")?;
        buildah_copy(container_id, tmp_path, PREPARE_ROOT_CONF_PATH)
            .context("Injecting prepare-root.conf into container")?;
    }

    set_image_labels(container_id, info).context("Setting OCI image labels")?;

    Ok(())
}

/// Run `buildah copy <container> <src> <dest>`.
fn buildah_copy(container_id: &str, src: &str, dest: &str) -> Result<()> {
    let status = std::process::Command::new(bootc_utils::buildah_bin())
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
