//! # Package-mode to image-mode migration helpers
//!
//! This module implements the post-install state-preservation steps that make
//! `bootc install to-existing-root` useful for converting a live package-mode
//! (RPM/DEB) system to a bootc image-mode deployment without losing data.
//!
//! `/var` and `/etc` migration are triggered by passing `--preserve-var` and/or
//! `--merge-etc` to `bootc install to-existing-root`. They run **after** the
//! core install (ostree deploy + bootupd) completes but **before** the first
//! reboot. Package boot rollback validation runs before `/boot` cleanup; the
//! opt-in install path leaves the existing package entry and its files in place
//! and verifies them again after bootupd.
//!
//! ## `/var` preservation (`--preserve-var`)
//!
//! After a plain `bootc install to-existing-root`, the new deployment's `/var`
//! is bound from `<sysroot>/ostree/deploy/<stateroot>/var/` — an initially-empty
//! directory.  The old package-mode `/var` is stranded at `<root_path>/var`
//! (inside the container, the host root is mounted at `root_path`).
//!
//! Two strategies are tried in order:
//!
//! - **Strategy C — reflink copy** (btrfs / XFS with reflinks): `cp --reflink=always`
//!   performs an instantaneous copy-on-write clone.  No extra disk space is used
//!   until data diverges.
//!
//! - **Strategy D — plain copy** (ext4 and other non-reflink filesystems):
//!   `cp -a` copies each non-ephemeral subdirectory of `/var` into the new
//!   deployment's stateroot `var/`.  This is correct but slow for large `/var`
//!   trees, and **unsafe for live databases** (see the known-limitation comment
//!   on `preserve_var_copy`).
//!
//! Exclusions such as `tmp`, `log/journal`, `lib/containers`, or `lib/rpm`
//! can be supplied through configuration or repeated `--preserve-var-skip`
//! options.
//!
//! ## `/etc` merge (`--merge-etc`)
//!
//! A plain `bootc install to-existing-root` populates the new deployment's `/etc`
//! from the image.  The running system's admin customisations (NIC profiles, SSH
//! host keys, secrets, etc.) end up at `<root_path>/etc` but are not applied to
//! the new `/etc`.
//!
//! `--merge-etc` runs the 3-way merge from the `etc-merge` crate at install time:
//!
//! | Input | Source |
//! |-------|--------|
//! | A — pristine baseline | `<deploy_dir>/usr/etc` (image's shipped defaults) |
//! | B — current live      | `<root_path>/etc` (running admin customisations)  |
//! | C — new deployment    | `<deploy_dir>/etc` (deploy target, written by image) |
//!
//! The diff A→B captures everything the admin changed relative to the image
//! defaults and applies those changes onto C.  Machine-specific files (SSH host
//! keys, machine-id, NIC profiles) are included because they are precisely what
//! needs to be transferred to make the migrated system functional.
//!
//! Rollback preservation is intentionally independent from `/var` migration.

mod package_boot;

pub(crate) use package_boot::PackageBootEntry;

use std::path::{Component, Path};
use std::{
    ffi::OsStr,
    os::fd::AsFd,
    path::PathBuf,
    process::{Command, Stdio},
    sync::Arc,
};

use anyhow::{Context, Result};
use cap_std_ext::cap_std::ambient_authority;
use cap_std_ext::cap_std::fs::Dir as CapStdDir;
use cap_std_ext::cap_std::io_lifetimes::AsFilelike;
use cap_std_ext::cmdext::{CapStdExtCommandExt, CmdFds};
use composefs_ctl::composefs::generic_tree::{FileSystem, Stat};
use etc_merge::{compute_diff_without_deletions, merge, traverse_etc};
use fn_error_context::context;

use crate::store::reflink::supports_reflink;

// ── Top-level entry points ────────────────────────────────────────────────────

/// Run the 3-way `/etc` merge on the new deployment.
///
/// Inputs:
///   - **A (pristine)** = `<deploy_dir>/usr/etc`
///   - **B (current)**  = `<root_path>/etc`
///   - **C (new)**      = `<deploy_dir>/etc`
///
/// `root_path` is the host root as seen from inside the install container.
#[context("Running 3-way /etc merge into new deployment")]
pub(crate) fn merge_etc_into_deployment(root_path: &Path, deploy_dir: &Path) -> Result<()> {
    let deploy_usr_etc = deploy_dir.join("usr/etc");
    let deploy_etc = deploy_dir.join("etc");
    let host_etc = root_path.join("etc");

    merge_etc(&host_etc, &deploy_usr_etc, &deploy_etc)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Preserve the running `/var` into the new deployment's `var/` directory.
///
/// `src_var` is the running system's `/var` (at `<root_path>/var`).
/// `new_var` is the new deployment's empty `var/` directory.
pub(crate) fn preserve_var(src_var: &Path, new_var: &Path, exclusions: &[String]) -> Result<()> {
    let src_var = CapStdDir::open_ambient_dir(src_var, ambient_authority())
        .with_context(|| format!("Opening source {}", src_var.display()))?;
    let new_var = open_or_create_var_dir(new_var)?;

    let mut effective_exclusions = ["tmp", "cache", "log/journal", "lib/containers"]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
    effective_exclusions.extend(exclusions.iter().cloned());

    if supports_reflink(&src_var, &new_var)? {
        println!("  Filesystem supports reflinks — using copy-on-write clone (Strategy C)");
        preserve_var_reflink(&src_var, &new_var, &effective_exclusions)
    } else {
        println!("  Filesystem does not support reflinks — falling back to full copy (Strategy D)");
        preserve_var_copy(&src_var, &new_var, &effective_exclusions)
    }
}

/// Open the destination through its parent capability, creating just the
/// final `/var` directory if needed. The caller-supplied parent is the initial
/// ambient authority; all subsequent traversal is relative to opened handles.
fn open_or_create_var_dir(path: &Path) -> Result<CapStdDir> {
    let parent = path.parent().context("Destination /var has no parent")?;
    let name = path
        .file_name()
        .context("Destination /var has no final path component")?;
    let parent = CapStdDir::open_ambient_dir(parent, ambient_authority())
        .with_context(|| format!("Opening destination parent {}", parent.display()))?;
    match parent.create_dir(name) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            anyhow::ensure!(
                parent.symlink_metadata(name)?.is_dir(),
                "Destination {} exists and is not a directory",
                path.display()
            );
        }
        Err(e) => return Err(e).with_context(|| format!("Creating {}", path.display())),
    }
    parent
        .open_dir(name)
        .with_context(|| format!("Opening destination {}", path.display()))
}

pub(crate) fn deployment_var_path(deploy_dir: &Path) -> Result<std::path::PathBuf> {
    let deploy_parent = deploy_dir
        .parent()
        .context("Deployment path has no parent")?;
    let stateroot = deploy_parent
        .parent()
        .context("Deployment path has no stateroot")?;
    Ok(stateroot.join("var"))
}

/// Run GNU cp with directory handles inherited as fixed descriptors. Each
/// operand is `/proc/self/fd/N[/entry]`; cp's top-level paths therefore remain
/// rooted in the opened capabilities even if the original paths are renamed.
/// GNU cp still performs its own recursive traversal beneath each operand, so
/// this is not race-safe against a concurrent writer replacing entries while
/// the copy is in progress.
fn run_cp(
    src_root: &CapStdDir,
    src_names: &[&OsStr],
    dst_root: &CapStdDir,
    dst_name: Option<&OsStr>,
    options: &[&str],
    stdout: Stdio,
    stderr: Stdio,
) -> Result<std::process::ExitStatus> {
    let src_fd = Arc::new(
        src_root
            .as_filelike_view::<std::fs::File>()
            .as_fd()
            .try_clone_to_owned()?,
    );
    let dst_fd = Arc::new(
        dst_root
            .as_filelike_view::<std::fs::File>()
            .as_fd()
            .try_clone_to_owned()?,
    );
    let mut fds = CmdFds::new();
    fds.take_fd_n(src_fd, 3);
    fds.take_fd_n(dst_fd, 4);

    let src_paths = src_names.iter().map(|name| {
        let mut path = PathBuf::from("/proc/self/fd/3");
        path.push(name);
        path
    });
    let mut dst_path = PathBuf::from("/proc/self/fd/4");
    if let Some(name) = dst_name {
        dst_path.push(name);
    }

    let status = Command::new("cp")
        .args(options)
        .arg("--")
        .args(src_paths)
        .arg(dst_path)
        .stdout(stdout)
        .stderr(stderr)
        .take_fds(fds)
        .status()
        .context("Running cp")?;
    Ok(status)
}

/// Strategy C: reflink-copy each top-level entry under `src_var` into `new_var`.
///
/// Skips well-known ephemeral subdirectories.
#[context("Reflink-copying /var into new deployment (Strategy C)")]
fn preserve_var_reflink(
    src_var: &CapStdDir,
    new_var: &CapStdDir,
    exclusions: &[String],
) -> Result<()> {
    let entries = src_var.read_dir(".").context("Reading source /var")?;

    for entry in entries {
        let entry = entry.context("Reading entry in source /var")?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if exclusions.iter().any(|s| name.to_str() == Some(s.as_str())) {
            println!("    Skipping {} (ephemeral)", name_str);
            continue;
        }

        let skips = nested_exclusions(&name, exclusions);
        if !skips.is_empty() && src_var.symlink_metadata(&name)?.is_dir() {
            copy_dir_skip_subdir(src_var, new_var, &name, &skips, true)?;
            continue;
        }

        println!("    Reflink-copying {} → {}", name_str, name_str);
        let status = run_cp(
            src_var,
            &[&name],
            new_var,
            None,
            &["--reflink=always", "-a", "--no-clobber"],
            Stdio::inherit(),
            Stdio::inherit(),
        )?;

        anyhow::ensure!(
            status.success(),
            "cp --reflink=always failed for {}",
            name_str
        );
    }

    println!("  /var reflink copy complete.");
    Ok(())
}

/// Copy a directory recursively, skipping selected child directories.
///
/// Used by both Strategy C and Strategy D to copy `var/log/` while excluding
/// `var/log/journal/`.  `reflink` selects whether `cp --reflink=always` or
/// plain `cp -a` is used. Included sibling entries are passed in one cp process
/// so hardlinks between them remain linked. As in the previous implementation,
/// this filtered path does not copy metadata from the containing directory
/// itself; ordinary (unfiltered) entries are copied wholly by cp -a.
fn copy_dir_skip_subdir(
    src_root: &CapStdDir,
    dst_root: &CapStdDir,
    dir_name: &OsStr,
    skip_names: &[&str],
    reflink: bool,
) -> Result<()> {
    let src = open_child_dir_nofollow(src_root, dir_name)
        .with_context(|| format!("Opening /var/{}", dir_name.to_string_lossy()))?;
    let dst = open_or_create_child_dir(dst_root, dir_name)?;
    let entries = src
        .read_dir(".")
        .context("Reading excluded-copy source directory")?;

    let mut included_entries = Vec::new();
    for entry in entries {
        let entry = entry.context("Reading entry in source child directory")?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if skip_names.iter().any(|skip| name == OsStr::new(skip)) {
            println!(
                "    Skipping {}/{} (ephemeral)",
                dir_name.to_string_lossy(),
                name_str
            );
            continue;
        }
        included_entries.push(name);
    }

    if !included_entries.is_empty() {
        let names = included_entries
            .iter()
            .map(|name| name.as_os_str())
            .collect::<Vec<_>>();
        let options = if reflink {
            &["--reflink=always", "-a", "--no-clobber"][..]
        } else {
            &["-a", "--no-clobber"][..]
        };
        let status = run_cp(
            &src,
            &names,
            &dst,
            None,
            options,
            Stdio::inherit(),
            Stdio::inherit(),
        )?;

        anyhow::ensure!(
            status.success(),
            "cp failed while copying {}",
            dir_name.to_string_lossy()
        );
    }

    Ok(())
}

/// Strategy D: plain recursive copy of `/var` for filesystems without reflink support.
///
/// # Known limitation
///
/// This performs a full `cp -a` of every included subdirectory of the running
/// `/var` into the new deployment's ostree stateroot `var/`.  For most workloads
/// this is fine, but it is **unsafe for databases and other applications that
/// keep open write handles into `/var`** (e.g. PostgreSQL in `/var/lib/pgsql`,
/// MySQL/MariaDB in `/var/lib/mysql`, SQLite databases under `/var/lib/*`).
/// Copying a live database with `cp -a` will almost certainly produce a
/// corrupted copy.
///
/// The correct fix is to run this migration only after stopping all stateful
/// services that write to `/var`, or — better — to migrate the filesystem to
/// btrfs or XFS (which support reflinks, Strategy C) so that the copy is
/// instantaneous and atomic from the kernel's perspective.
///
/// A future improvement would be to accept a user-supplied exclusion list so
/// that specific high-risk directories (e.g. `/var/lib/pgsql`) can be skipped
/// and migrated manually.  For now, operators are responsible for stopping
/// affected services before running `bootc install to-existing-root --preserve-var`
/// on ext4 (or other non-reflink) filesystems.
///
/// See: <https://github.com/bootc-dev/bootc/issues/2220>
#[context("Copying /var into new deployment (Strategy D)")]
fn preserve_var_copy(
    src_var: &CapStdDir,
    new_var: &CapStdDir,
    exclusions: &[String],
) -> Result<()> {
    let entries = src_var.read_dir(".").context("Reading source /var")?;

    for entry in entries {
        let entry = entry.context("Reading entry in source /var")?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if exclusions.iter().any(|s| name.to_str() == Some(s.as_str())) {
            println!("    Skipping {} (ephemeral)", name_str);
            continue;
        }

        let skips = nested_exclusions(&name, exclusions);
        if !skips.is_empty() && src_var.symlink_metadata(&name)?.is_dir() {
            copy_dir_skip_subdir(src_var, new_var, &name, &skips, false)?;
            continue;
        }

        println!("    Copying {} → {}", name_str, name_str);
        let status = run_cp(
            src_var,
            &[&name],
            new_var,
            None,
            &["-a", "--no-clobber"],
            Stdio::inherit(),
            Stdio::inherit(),
        )?;

        anyhow::ensure!(status.success(), "cp -a failed for {}", name_str);
    }

    println!("  /var copy complete.");
    Ok(())
}

fn nested_exclusions<'a>(name: &OsStr, exclusions: &'a [String]) -> Vec<&'a str> {
    let Some(name) = name.to_str() else {
        return Vec::new();
    };
    let prefix = format!("{name}/");
    exclusions
        .iter()
        .filter_map(|path| {
            path.strip_prefix(&prefix)
                .filter(|rest| !rest.contains('/'))
        })
        .collect()
}

/// Open a single source child directory without following a symlink swapped
/// into place after the caller's metadata check. The entry name comes directly
/// from `read_dir`, so this is one path component beneath the capability root.
fn open_child_dir_nofollow(parent: &CapStdDir, name: &OsStr) -> std::io::Result<CapStdDir> {
    use rustix::fs::{Mode, OFlags};

    let fd = rustix::fs::openat(
        parent.as_filelike_view::<std::fs::File>().as_fd(),
        name,
        OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY,
        Mode::empty(),
    )?;
    Ok(CapStdDir::from_std_file(std::fs::File::from(fd)))
}

fn open_or_create_child_dir(parent: &CapStdDir, name: &OsStr) -> Result<CapStdDir> {
    match parent.create_dir(name) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            anyhow::ensure!(
                parent.symlink_metadata(name)?.is_dir(),
                "Destination child {} exists and is not a directory",
                name.to_string_lossy()
            );
        }
        Err(e) => return Err(e).context("Creating destination child directory"),
    }
    parent
        .open_dir(name)
        .context("Opening destination child directory")
}

/// Validate exclusions before touching either tree. Paths are relative to `/var`.
pub(crate) fn validate_exclusions(exclusions: &[String]) -> Result<Vec<String>> {
    exclusions
        .iter()
        .map(|path| {
            let path = path.trim_end_matches('/');
            anyhow::ensure!(!path.is_empty(), "empty /var exclusion");
            anyhow::ensure!(
                Path::new(path)
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
                "invalid /var exclusion {path:?}; expected a relative path"
            );
            anyhow::ensure!(
                Path::new(path).components().count() <= 2,
                "unsupported nested /var exclusion {path:?}; use at most one child path"
            );
            Ok(path.to_string())
        })
        .collect()
}

/// 3-way `/etc` merge.
///
/// - `host_etc`       = `<root_path>/etc`      (running system, input B)
/// - `deploy_usr_etc` = `<deploy_dir>/usr/etc` (image defaults, input A)
/// - `deploy_etc`     = `<deploy_dir>/etc`     (new deploy target, input C)
#[context("Merging running /etc into new deployment")]
fn merge_etc(host_etc: &Path, deploy_usr_etc: &Path, deploy_etc: &Path) -> Result<()> {
    let pristine_fd = CapStdDir::open_ambient_dir(deploy_usr_etc, ambient_authority())
        .with_context(|| format!("Opening pristine etc: {}", deploy_usr_etc.display()))?;
    let current_fd = CapStdDir::open_ambient_dir(host_etc, ambient_authority())
        .with_context(|| format!("Opening running etc: {}", host_etc.display()))?;
    let new_fd = CapStdDir::open_ambient_dir(deploy_etc, ambient_authority())
        .with_context(|| format!("Opening deploy etc: {}", deploy_etc.display()))?;

    let (pristine_tree, current_tree, new_tree_opt) =
        traverse_etc(&pristine_fd, &current_fd, Some(&new_fd))
            .context("Traversing /etc trees for 3-way merge")?;

    let new_tree = new_tree_opt.unwrap_or_else(|| FileSystem::new(Stat::uninitialized()));

    let diff = compute_diff_without_deletions(&pristine_tree, &current_tree, &new_tree)
        .context("Computing /etc diff")?;

    println!("  /etc diff (changes being applied from running system):");
    etc_merge::print_diff(&diff, &mut std::io::stdout());

    merge(&current_fd, &current_tree, &new_fd, &new_tree, &diff)
        .context("Applying /etc 3-way merge")?;

    println!("  /etc merge complete.");
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use cap_std_ext::dirext::CapStdExtDirExt;
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt, symlink},
        time::{Duration, SystemTime},
    };

    fn cap_dir(path: &Path) -> CapStdDir {
        CapStdDir::open_ambient_dir(path, ambient_authority()).unwrap()
    }

    #[test]
    fn exclusions_are_relative_and_normalized() {
        assert_eq!(
            validate_exclusions(&["lib/rpm/".into()]).unwrap(),
            ["lib/rpm"]
        );
        assert!(validate_exclusions(&["../etc".into()]).is_err());
        assert!(validate_exclusions(&["/etc".into()]).is_err());
        assert!(validate_exclusions(&["lib/containers/storage".into()]).is_err());
    }

    #[test]
    fn deployment_var_is_stateroot_var() {
        let deploy = Path::new("/sysroot/ostree/deploy/default/deploy/checksum.0");
        assert_eq!(
            deployment_var_path(deploy).unwrap(),
            Path::new("/sysroot/ostree/deploy/default/var")
        );
    }

    #[test]
    fn plain_copy_skips_multiple_children_under_parent() -> Result<()> {
        let exclusions = ["tmp".into(), "log/journal".into(), "log/private".into()];
        assert_eq!(
            nested_exclusions(OsStr::new("log"), &exclusions),
            ["journal", "private"]
        );

        let temp = tempfile::tempdir()?;
        let src_path = temp.path().join("src");
        let dst_path = temp.path().join("dst");
        fs::create_dir_all(src_path.join("tmp"))?;
        fs::create_dir_all(src_path.join("log/journal"))?;
        fs::create_dir_all(src_path.join("log/private"))?;
        fs::create_dir_all(src_path.join("log/keep"))?;
        fs::write(src_path.join("tmp/ignored"), b"tmp")?;
        fs::write(src_path.join("log/journal/ignored"), b"journal")?;
        fs::write(src_path.join("log/private/ignored"), b"private")?;
        fs::write(src_path.join("log/keep/preserved"), b"keep")?;
        fs::hard_link(
            src_path.join("log/keep/preserved"),
            src_path.join("log/keep/preserved-hardlink"),
        )?;
        fs::create_dir(&dst_path)?;

        preserve_var_copy(&cap_dir(&src_path), &cap_dir(&dst_path), &exclusions)?;

        assert!(!dst_path.join("tmp").exists());
        assert!(!dst_path.join("log/journal").exists());
        assert!(!dst_path.join("log/private").exists());
        assert_eq!(fs::read(dst_path.join("log/keep/preserved"))?, b"keep");
        assert_eq!(
            fs::metadata(dst_path.join("log/keep/preserved"))?.ino(),
            fs::metadata(dst_path.join("log/keep/preserved-hardlink"))?.ino()
        );
        Ok(())
    }

    #[test]
    fn reflink_strategy_skips_multiple_children_under_parent() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let src_path = temp.path().join("src");
        let dst_path = temp.path().join("dst");
        fs::create_dir_all(src_path.join("log/journal"))?;
        fs::create_dir_all(src_path.join("log/private"))?;
        fs::create_dir_all(src_path.join("log/keep"))?;
        fs::write(src_path.join("log/journal/ignored"), b"journal")?;
        fs::write(src_path.join("log/private/ignored"), b"private")?;
        fs::write(src_path.join("log/keep/preserved"), b"keep")?;
        fs::create_dir(&dst_path)?;
        let src = cap_dir(&src_path);
        let dst = cap_dir(&dst_path);
        let supports_reflink = supports_reflink(&src, &dst)?;
        if !supports_reflink {
            fs::remove_file(src_path.join("log/keep/preserved"))?;
            fs::remove_dir(src_path.join("log/keep"))?;
        }

        // Exercise Strategy C's exclusion routing on every filesystem and, if
        // available, its reflink copy of a retained sibling.
        preserve_var_reflink(&src, &dst, &["log/journal".into(), "log/private".into()])?;

        assert!(!dst_path.join("log/journal").exists());
        assert!(!dst_path.join("log/private").exists());
        assert_eq!(
            dst_path.join("log/keep/preserved").exists(),
            supports_reflink
        );
        Ok(())
    }

    #[test]
    fn copy_preserves_symlink_without_traversing_target() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let src_path = temp.path().join("src");
        let dst_path = temp.path().join("dst");
        let outside = temp.path().join("outside");
        fs::create_dir(&src_path)?;
        fs::create_dir(&dst_path)?;
        fs::create_dir(&outside)?;
        fs::write(outside.join("sentinel"), b"outside")?;
        symlink(&outside, src_path.join("escape"))?;

        preserve_var_copy(
            &cap_dir(&src_path),
            &cap_dir(&dst_path),
            &["escape/sentinel".into()],
        )?;

        let copied = fs::symlink_metadata(dst_path.join("escape"))?;
        assert!(copied.file_type().is_symlink());
        assert_eq!(fs::read_link(dst_path.join("escape"))?, outside);
        // The target remains outside the destination tree; it was not copied.
        assert!(!dst_path.join("sentinel").exists());
        Ok(())
    }

    #[test]
    fn copy_preserves_archive_metadata_and_hardlinks() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let src_path = temp.path().join("src");
        let dst_path = temp.path().join("dst");
        let src_state = src_path.join("state");
        fs::create_dir_all(&src_state)?;
        fs::create_dir(&dst_path)?;
        fs::write(src_state.join("one"), b"contents")?;
        fs::hard_link(src_state.join("one"), src_state.join("two"))?;
        fs::set_permissions(&src_state, fs::Permissions::from_mode(0o751))?;
        fs::set_permissions(src_state.join("one"), fs::Permissions::from_mode(0o640))?;
        let src_cap = cap_dir(&src_path);
        let dst_cap = cap_dir(&dst_path);
        let xattr_supported = src_cap
            .setxattr("state/one", "user.bootc_migrate_test", b"xattr")
            .is_ok();
        let expected_mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        fs::File::open(&src_state)?.set_times(fs::FileTimes::new().set_modified(expected_mtime))?;

        preserve_var_copy(&src_cap, &dst_cap, &[])?;

        let copied_dir = fs::metadata(dst_path.join("state"))?;
        let copied_one = fs::metadata(dst_path.join("state/one"))?;
        let copied_two = fs::metadata(dst_path.join("state/two"))?;
        assert_eq!(copied_dir.permissions().mode() & 0o7777, 0o751);
        assert_eq!(copied_one.permissions().mode() & 0o7777, 0o640);
        assert_eq!(copied_dir.modified()?, expected_mtime);
        assert_eq!(copied_one.ino(), copied_two.ino());
        assert_eq!(copied_one.nlink(), 2);
        assert_eq!(copied_one.uid(), fs::metadata(src_state.join("one"))?.uid());
        assert_eq!(copied_one.gid(), fs::metadata(src_state.join("one"))?.gid());
        if xattr_supported {
            assert_eq!(
                dst_cap.getxattr("state/one", "user.bootc_migrate_test")?,
                Some(b"xattr".to_vec())
            );
        }
        Ok(())
    }

    #[test]
    fn copy_keeps_existing_collisions_untouched() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let src_path = temp.path().join("src");
        let dst_path = temp.path().join("dst");
        fs::create_dir_all(src_path.join("state"))?;
        fs::create_dir_all(dst_path.join("state"))?;
        fs::write(src_path.join("state/existing"), b"source")?;
        fs::write(src_path.join("state/new"), b"new")?;
        fs::write(dst_path.join("state/existing"), b"destination")?;

        preserve_var_copy(&cap_dir(&src_path), &cap_dir(&dst_path), &[])?;

        assert_eq!(fs::read(dst_path.join("state/existing"))?, b"destination");
        assert_eq!(fs::read(dst_path.join("state/new"))?, b"new");
        Ok(())
    }
}
