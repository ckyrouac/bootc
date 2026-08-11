use anyhow::{anyhow, bail, Context, Result};
use camino::Utf8Path;
use fn_error_context::context;
use std::process::Command;

use crate::task::Task;
use bootc_utils::CommandRunExt;

/// The name of the mountpoint for efi (as a subdirectory of /boot, or at the toplevel)
pub(crate) const EFI_DIR: &str = "efi";
#[cfg(feature = "install-to-disk")]
pub(crate) const ESP_GUID: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
#[cfg(feature = "install-to-disk")]
pub(crate) const PREPBOOT_GUID: &str = "9E1A2D38-C612-4316-AA26-8B49521E5A8B";
#[cfg(feature = "install-to-disk")]
pub(crate) const PREPBOOT_LABEL: &str = "PowerPC-PReP-boot";
#[cfg(target_arch = "powerpc64")]
/// We make a best-effort to support MBR partitioning too.
pub(crate) const PREPBOOT_MBR_TYPE: &str = "41";

/// Check whether the target bootupd supports `--filesystem`.
///
/// Runs `bootupctl backend install --help` and looks for `--filesystem` in the
/// output. This allows us to use the new multi-device-aware `--filesystem` flag
/// when available, and fall back to the legacy `--device` flag otherwise.
fn bootupd_supports_filesystem() -> Result<bool> {
    let output = Command::new("bootupctl")
        .args(["backend", "install", "--help"])
        .log_debug()
        .run_get_string()?;

    let use_filesystem = output.contains("--filesystem");

    if use_filesystem {
        tracing::debug!("bootupd supports --filesystem");
    } else {
        tracing::debug!("bootupd does not support --filesystem, falling back to --device");
    }

    Ok(use_filesystem)
}

/// Install the bootloader via bootupd.
///
/// When the target bootupd supports `--filesystem` we pass it pointing at the
/// root filesystem mount so that bootupd can resolve the backing device(s) itself
/// via `lsblk`. This enables multi-device support (Intel VROC RAID, multipath).
///
/// For older bootupd versions that lack `--filesystem` we fall back to the
/// legacy `--device <device_path> <rootfs>` invocation, which only supports
/// a single backing device.
#[context("Installing bootloader")]
pub(crate) fn install_via_bootupd(
    device: &bootc_blockdev::Device,
    rootfs: &Utf8Path,
    configopts: &crate::install::InstallConfigOpts,
) -> Result<()> {
    let verbose = std::env::var_os("BOOTC_BOOTLOADER_DEBUG").map(|_| "-vvvv");
    // bootc defaults to only targeting the platform boot method.
    let bootupd_opts = (!configopts.generic_image).then_some(["--update-firmware", "--auto"]);

    println!("Installing bootloader via bootupd");

    let mut args: Vec<&str> = vec!["backend", "install", "--write-uuid"];
    if let Some(v) = verbose {
        args.push(v);
    }
    if let Some(ref opts) = bootupd_opts {
        args.extend(opts.iter().copied());
    }

    // Probe whether the installed bootupd supports `--filesystem`.
    // When it does, pass `--filesystem <rootfs>` so bootupd resolves the
    // backing device(s) itself — this is required for multi-device setups
    // (Intel VROC RAID, multipath) where there is more than one parent disk.
    //
    // When it doesn't, fall back to `--device <whole_disk> <rootfs>`.
    // For --device we need the whole-disk path (e.g. /dev/vda), so we call
    // require_single_root() — older bootupd doesn't support multiple devices.
    if bootupd_supports_filesystem().context("Probing bootupd --filesystem support")? {
        tracing::debug!("bootupd supports --filesystem, using multi-device-capable path");
        // --filesystem <rootfs> <rootfs>  (rootfs appears twice: once as the flag
        // argument for block device resolution, and once as the install root)
        let rootfs_str = rootfs.as_str();
        args.extend(["--filesystem", rootfs_str]);
        args.push(rootfs_str);
        Task::new("Running bootupctl to install bootloader", "bootupctl")
            .args(args)
            .verbose()
            .run()
    } else {
        // Legacy path: find the single whole-disk backing device.
        #[cfg(all(target_arch = "powerpc64", feature = "install-to-disk"))]
        {
            // On powerpc64, bootupd needs the PReP partition, not the whole disk.
            let prep_path = get_prep_device(device)?;
            args.extend(["--device", prep_path.as_str(), rootfs.as_str()]);
            return Task::new("Running bootupctl to install bootloader", "bootupctl")
                .args(args)
                .verbose()
                .run();
        }

        #[cfg(not(all(target_arch = "powerpc64", feature = "install-to-disk")))]
        {
            let root_device_path = device
                .require_single_root()
                .context("Finding single root device for bootupd --device")?
                .path();
            tracing::debug!(
                "bootupd does not support --filesystem, falling back to --device {root_device_path}"
            );
            args.extend(["--device", &root_device_path, rootfs.as_str()]);
            Task::new("Running bootupctl to install bootloader", "bootupctl")
                .args(args)
                .verbose()
                .run()
        }
    }
}

/// Find the PReP boot device to pass to bootupd on powerpc64.
///
/// On powerpc64, bootupd requires the PReP partition path rather than the
/// whole disk. We walk all root devices and look for a partition with the
/// PReP GUID or MBR type.
#[cfg(all(target_arch = "powerpc64", feature = "install-to-disk"))]
fn get_prep_device(device: &bootc_blockdev::Device) -> Result<String> {
    let roots = device
        .find_all_roots()
        .context("Finding root devices for PReP lookup")?;
    for root in &roots {
        if let Some(children) = root.children.as_ref() {
            for child in children {
                if let Some(ref pt) = child.parttype {
                    if pt.eq_ignore_ascii_case(PREPBOOT_GUID) || pt == PREPBOOT_MBR_TYPE {
                        return Ok(child.path());
                    }
                }
                // Also match by label for MBR layouts
                if child.partlabel.as_deref() == Some(PREPBOOT_LABEL) {
                    return Ok(child.path());
                }
            }
        }
    }
    anyhow::bail!(
        "Failed to find PReP partition with GUID {PREPBOOT_GUID} among root device(s)"
    )
}

#[context("Installing bootloader using zipl")]
pub(crate) fn install_via_zipl(device: &bootc_blockdev::Device, boot_uuid: &str) -> Result<()> {
    // On s390x, zipl only supports a single backing device.
    let root_device = device
        .require_single_root()
        .context("Finding single root device for zipl")?;

    // Identify the target boot partition from UUID
    let fs = crate::mount::inspect_filesystem_by_uuid(boot_uuid)?;
    let boot_dir = Utf8Path::new(&fs.target);
    let maj_min = fs.maj_min;

    // Ensure that the found partition is a part of the target device
    let device_path = root_device.path();

    let partitions = bootc_blockdev::list_dev(Utf8Path::new(&device_path))?
        .children
        .with_context(|| format!("no partition found on {device_path}"))?;
    let boot_part = partitions
        .iter()
        .find(|part| part.maj_min.as_deref() == Some(maj_min.as_str()))
        .with_context(|| format!("partition device {maj_min} is not on {device_path}"))?;
    let boot_part_offset = boot_part.start.unwrap_or(0);

    // Find exactly one BLS configuration under /boot/loader/entries
    // TODO: utilize the BLS parser in ostree
    let bls_dir = boot_dir.join("boot/loader/entries");
    let bls_entry = bls_dir
        .read_dir_utf8()?
        .try_fold(None, |acc, e| -> Result<_> {
            let e = e?;
            let name = Utf8Path::new(e.file_name());
            if let Some("conf") = name.extension() {
                if acc.is_some() {
                    bail!("more than one BLS configurations under {bls_dir}");
                }
                Ok(Some(e.path().to_owned()))
            } else {
                Ok(None)
            }
        })?
        .with_context(|| format!("no BLS configuration under {bls_dir}"))?;

    let bls_path = bls_dir.join(bls_entry);
    let bls_conf =
        std::fs::read_to_string(&bls_path).with_context(|| format!("reading {bls_path}"))?;

    let mut kernel = None;
    let mut initrd = None;
    let mut options = None;

    for line in bls_conf.lines() {
        match line.split_once(char::is_whitespace) {
            Some(("linux", val)) => kernel = Some(val.trim().trim_start_matches('/')),
            Some(("initrd", val)) => initrd = Some(val.trim().trim_start_matches('/')),
            Some(("options", val)) => options = Some(val.trim()),
            _ => (),
        }
    }

    let kernel = kernel.ok_or_else(|| anyhow!("missing 'linux' key in default BLS config"))?;
    let initrd = initrd.ok_or_else(|| anyhow!("missing 'initrd' key in default BLS config"))?;
    let options = options.ok_or_else(|| anyhow!("missing 'options' key in default BLS config"))?;

    let image = boot_dir.join(kernel).canonicalize_utf8()?;
    let ramdisk = boot_dir.join(initrd).canonicalize_utf8()?;

    // Execute the zipl command to install bootloader
    let zipl_desc = format!("running zipl to install bootloader on {device_path}");
    let zipl_task = Task::new(&zipl_desc, "zipl")
        .args(["--target", boot_dir.as_str()])
        .args(["--image", image.as_str()])
        .args(["--ramdisk", ramdisk.as_str()])
        .args(["--parameters", options])
        .args(["--targetbase", device_path.as_str()])
        .args(["--targettype", "SCSI"])
        .args(["--targetblocksize", "512"])
        .args(["--targetoffset", &boot_part_offset.to_string()])
        .args(["--add-files", "--verbose"]);
    zipl_task.verbose().run().context(zipl_desc)
}
