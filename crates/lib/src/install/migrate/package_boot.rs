//! Validation for retaining a package-mode GRUB/BLS boot entry.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::parsers::bls_config::{BLSConfigType, parse_bls_config};

/// A validated running-kernel BLS entry whose files must survive install.
#[derive(Debug)]
pub(crate) struct PackageBootEntry {
    boot_path: PathBuf,
    entry_path: PathBuf,
    entry_contents: String,
    title: String,
    assets: Vec<BootAsset>,
}

#[derive(Debug)]
struct BootAsset {
    path: PathBuf,
    sha256: Vec<u8>,
}

impl PackageBootEntry {
    /// Validate the running package kernel and the GRUB configuration before
    /// `clean_boot_directories` can mutate `/boot`.
    pub(crate) fn preflight(root: &Path, kernel: &str, cmdline: &str) -> Result<Self> {
        let running = RunningCmdline::parse(cmdline)?;
        let boot_path = fs::canonicalize(root.join("boot"))
            .context("Canonicalizing package-mode /boot directory")?;
        ensure!(
            fs::metadata(&boot_path)
                .with_context(|| format!("Inspecting boot root {}", boot_path.display()))?
                .is_dir(),
            "Package-mode boot root is not a directory: {}",
            boot_path.display()
        );
        let grub_config = validate_grub_bls(&boot_path)?;

        let entries_path = boot_path.join("loader/entries");
        let mut matches = Vec::new();
        let mut titles = Vec::new();
        for entry in fs::read_dir(&entries_path)
            .with_context(|| format!("Reading BLS entries in {}", entries_path.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("conf") {
                continue;
            }
            ensure!(
                fs::symlink_metadata(&path)
                    .with_context(|| format!("Inspecting BLS entry {}", path.display()))?
                    .is_file(),
                "BLS entry is not a regular file: {}",
                path.display()
            );
            let contents = fs::read_to_string(&path)
                .with_context(|| format!("Reading BLS entry {}", path.display()))?;
            validate_unique_singleton_directives(&contents)
                .with_context(|| format!("Validating BLS entry {}", path.display()))?;
            let config = parse_bls_config(&contents)
                .with_context(|| format!("Parsing BLS entry {}", path.display()))?;
            reject_grub_variable_asset_paths(&config)?;
            if let Some(title) = config.title.as_ref() {
                titles.push(title.clone());
            }
            if config.version().as_str() != kernel {
                continue;
            }

            let (linux, initrd, options) = match config.cfg_type {
                BLSConfigType::NonEFI {
                    linux,
                    initrd,
                    options,
                } => {
                    let Ok(linux) = normalize_boot_path(linux.as_str()) else {
                        continue;
                    };
                    if linux != running.kernel_path {
                        continue;
                    }
                    (linux, initrd, options)
                }
                BLSConfigType::EFI { key } => {
                    let path = match key {
                        crate::parsers::bls_config::EFIKey::Efi(path)
                        | crate::parsers::bls_config::EFIKey::Uki(path) => {
                            normalize_boot_path(path.as_str())
                        }
                    };
                    let Ok(path) = path else {
                        continue;
                    };
                    if path != running.kernel_path {
                        continue;
                    }
                    bail!(
                        "The selected running-kernel BLS entry is an EFI/UKI entry; only GRUB kernel/initrd entries are supported"
                    );
                }
                BLSConfigType::Unknown => continue,
            };
            ensure!(
                config
                    .title
                    .as_deref()
                    .is_some_and(|title| !title.trim().is_empty()),
                "Running package BLS entry has no non-empty title"
            );
            let title = config.title.as_ref().unwrap().clone();
            ensure!(
                !initrd.is_empty(),
                "Running package BLS entry has no initrd"
            );
            let options = options.context("Running package BLS entry has no options")?;
            let options = options.to_string();
            let options = if options.contains('$') {
                let kernelopts = read_grub_kernelopts(&grub_config)?;
                resolve_options(&options, Some(&kernelopts))?
            } else {
                resolve_options(&options, None)?
            };
            let entry_args = parse_args(&options)?;
            let entry_root = root_argument(&entry_args)?;
            ensure!(
                entry_root == running.root,
                "Running-kernel BLS entry root={entry_root} does not match running root={}",
                running.root
            );
            ensure!(
                entry_args == running.args,
                "Running-kernel BLS options do not match /proc/cmdline"
            );
            ensure!(
                !entry_args.iter().any(|arg| arg.starts_with("ostree=")),
                "Running package BLS entry is not a package-mode entry"
            );

            let mut assets = Vec::with_capacity(
                1 + initrd.len() + usize::from(config.extra.contains_key("devicetree")),
            );
            assets.push(linux);
            assets.extend(
                initrd
                    .iter()
                    .map(|path| normalize_boot_path(path.as_str()))
                    .collect::<Result<Vec<_>>>()?,
            );
            if let Some(devicetree) = config.extra.get("devicetree") {
                assets.push(normalize_boot_path(devicetree)?);
            }
            let assets = identify_assets(&boot_path, assets)?;
            matches.push((path, contents, title, assets));
        }

        ensure!(
            matches.len() == 1,
            "Expected one usable package-mode BLS entry matching the running kernel, command line, and root; found {}",
            matches.len()
        );
        let (entry_path, entry_contents, title, assets) = matches.pop().unwrap();
        ensure!(
            titles
                .iter()
                .filter(|existing| existing.as_str() == title)
                .count()
                == 1,
            "Package-mode BLS title is not unique: {title:?}"
        );
        Ok(Self {
            boot_path,
            entry_path,
            entry_contents,
            title,
            assets,
        })
    }

    /// Confirm OSTree and bootupd left the original package entry and all of
    /// its boot files available to the installed GRUB BLS loader.
    pub(crate) fn verify_retained(&self) -> Result<()> {
        validate_grub_bls(&self.boot_path)?;
        ensure!(
            fs::read_to_string(&self.entry_path)
                .with_context(|| format!("Reading {}", self.entry_path.display()))?
                == self.entry_contents,
            "Package-mode BLS entry changed or disappeared during install: {}",
            self.entry_path.display()
        );
        ensure_unique_title(&self.boot_path.join("loader/entries"), &self.title)?;
        verify_assets(&self.boot_path, &self.assets)
    }
}

struct RunningCmdline {
    args: Vec<String>,
    root: String,
    kernel_path: PathBuf,
}

impl RunningCmdline {
    fn parse(cmdline: &str) -> Result<Self> {
        let mut args = Vec::new();
        let mut boot_image = None;
        for arg in cmdline.split_whitespace() {
            ensure!(
                !arg.contains('$'),
                "Dynamic arguments in /proc/cmdline are unsupported"
            );
            if let Some(value) = arg.strip_prefix("BOOT_IMAGE=") {
                ensure!(
                    boot_image.replace(value).is_none(),
                    "Multiple BOOT_IMAGE arguments in /proc/cmdline"
                );
            } else {
                args.push(arg.to_owned());
            }
        }
        let root = root_argument(&args)?.to_owned();
        ensure!(
            !args
                .iter()
                .any(|arg| arg.starts_with("ostree=") || arg.starts_with("rd.ostree")),
            "--preserve-package-boot requires the currently running package-mode OS"
        );
        let boot_image = boot_image
            .context("Cannot identify the running kernel without BOOT_IMAGE in /proc/cmdline")?;
        // GRUB commonly prefixes BOOT_IMAGE with a device specifier, while
        // the BLS linux path is relative to the boot filesystem.
        let image_path = if boot_image.starts_with('(') {
            boot_image
                .split_once(')')
                .map(|(_, path)| path)
                .context("Invalid GRUB device prefix in BOOT_IMAGE")?
        } else {
            boot_image
        };
        let kernel_path = normalize_boot_path(image_path).context("Invalid BOOT_IMAGE path")?;
        Ok(Self {
            args,
            root,
            kernel_path,
        })
    }
}

fn parse_args(options: &str) -> Result<Vec<String>> {
    ensure!(
        !options
            .chars()
            .any(|c| matches!(c, '\n' | '\r' | '\0' | '\'' | '"')),
        "Quoted or multiline BLS options cannot be safely matched to /proc/cmdline"
    );
    let args = options
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    ensure!(!args.is_empty(), "Empty BLS options");
    ensure!(
        !args.iter().any(|arg| arg.contains('$')),
        "Unresolved dynamic variable in BLS options"
    );
    Ok(args)
}

fn root_argument(args: &[String]) -> Result<&str> {
    let roots = args
        .iter()
        .filter_map(|arg| arg.strip_prefix("root="))
        .collect::<Vec<_>>();
    ensure!(
        roots.len() == 1 && !roots[0].is_empty(),
        "Expected exactly one concrete root= argument"
    );
    Ok(roots[0])
}

fn resolve_options(options: &str, kernelopts: Option<&str>) -> Result<String> {
    if !options.contains('$') {
        return Ok(options.to_owned());
    }
    ensure!(
        matches!(options.trim(), "$kernelopts" | "${kernelopts}"),
        "Unsupported dynamic GRUB variables in BLS options: {options:?}"
    );
    let value = kernelopts.context("Cannot resolve dynamic $kernelopts in BLS options")?;
    ensure!(
        !value.is_empty() && !value.contains('$'),
        "GRUB kernelopts is empty or contains unresolved variables"
    );
    Ok(value.to_owned())
}

fn read_grub_kernelopts(grub_config: &Path) -> Result<String> {
    let grubenv = grub_config
        .parent()
        .context("GRUB configuration has no parent directory")?
        .join("grubenv");
    let output = Command::new("grub2-editenv")
        .arg(&grubenv)
        .arg("list")
        .output()
        .with_context(|| format!("Reading GRUB environment {}", grubenv.display()))?;
    ensure!(
        output.status.success(),
        "grub2-editenv failed to read {}",
        grubenv.display()
    );
    let output = String::from_utf8(output.stdout).context("grub2-editenv output is not UTF-8")?;
    parse_kernelopts(&output)
}

fn parse_kernelopts(output: &str) -> Result<String> {
    let values = output
        .lines()
        .filter_map(|line| line.strip_prefix("kernelopts="))
        .collect::<Vec<_>>();
    ensure!(
        values.len() == 1,
        "Expected one kernelopts value in GRUB environment"
    );
    let value = values[0].trim();
    ensure!(!value.is_empty(), "GRUB kernelopts is empty");
    Ok(value.to_owned())
}

fn validate_grub_bls(boot_path: &Path) -> Result<PathBuf> {
    let config = ["grub2/grub.cfg", "grub/grub.cfg"]
        .into_iter()
        .map(|path| boot_path.join(path))
        .find(|path| path.is_file())
        .context("No GRUB configuration found under /boot")?;
    let contents = fs::read_to_string(&config)
        .with_context(|| format!("Reading GRUB configuration {}", config.display()))?;
    ensure!(
        contents.lines().any(|line| line.trim() == "blscfg"),
        "GRUB configuration {} does not load BLS entries",
        config.display()
    );
    Ok(config)
}

fn normalize_boot_path(path: &str) -> Result<PathBuf> {
    reject_grub_variable_expansion(path, "boot asset")?;
    // BLS paths may include the mountpoint even though this root is already
    // the mounted boot filesystem. Keep traditional /vmlinuz paths relative
    // to that root, while treating /boot/vmlinuz the same way.
    let path = path.strip_prefix("/boot/").unwrap_or(path);
    let path = Path::new(path);
    let path = path.strip_prefix("/").unwrap_or(path);
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "Unsafe BLS boot asset path: {path:?}"
    );
    Ok(path.to_path_buf())
}

fn reject_grub_variable_expansion(path: &str, field: &str) -> Result<()> {
    ensure!(
        !path.contains('$'),
        "GRUB variable expansion in BLS {field} path is unsupported: {path:?}"
    );
    Ok(())
}

fn reject_grub_variable_asset_paths(config: &crate::parsers::bls_config::BLSConfig) -> Result<()> {
    match &config.cfg_type {
        BLSConfigType::NonEFI { linux, initrd, .. } => {
            reject_grub_variable_expansion(linux.as_str(), "linux")?;
            for path in initrd {
                reject_grub_variable_expansion(path.as_str(), "initrd")?;
            }
        }
        BLSConfigType::EFI { key } => {
            let path = match key {
                crate::parsers::bls_config::EFIKey::Efi(path)
                | crate::parsers::bls_config::EFIKey::Uki(path) => path,
            };
            reject_grub_variable_expansion(path.as_str(), "EFI")?;
        }
        BLSConfigType::Unknown => {}
    }
    if let Some(path) = config.extra.get("devicetree") {
        reject_grub_variable_expansion(path, "devicetree")?;
    }
    Ok(())
}

fn validate_unique_singleton_directives(contents: &str) -> Result<()> {
    let singleton_directives = [
        "title",
        "version",
        "machine-id",
        "sort-key",
        "architecture",
        "linux",
        "options",
        "efi",
        "uki",
        "devicetree",
    ];
    let mut seen = HashSet::new();
    for line in contents.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(directive) = line.split_ascii_whitespace().next() else {
            continue;
        };
        if singleton_directives.contains(&directive) {
            ensure!(
                seen.insert(directive),
                "Duplicate BLS {directive} directive"
            );
        }
        if matches!(directive, "efi" | "uki") {
            let alias = if directive == "efi" { "uki" } else { "efi" };
            ensure!(
                !seen.contains(alias),
                "BLS entry cannot contain both 'efi' and 'uki' directives"
            );
        }
    }
    // `initrd` is deliberately not treated as a singleton: the BLS format
    // permits an ordered list of initrd images, which GRUB loads together.
    Ok(())
}

fn identify_assets(boot_path: &Path, assets: Vec<PathBuf>) -> Result<Vec<BootAsset>> {
    assets
        .into_iter()
        .map(|asset| {
            let path = resolve_boot_asset(boot_path, &asset)?;
            let sha256 = hash_file(&path)?;
            Ok(BootAsset {
                path: asset,
                sha256,
            })
        })
        .collect()
}

fn verify_assets(boot_path: &Path, assets: &[BootAsset]) -> Result<()> {
    for asset in assets {
        let path = resolve_boot_asset(boot_path, &asset.path)?;
        ensure!(
            hash_file(&path)? == asset.sha256,
            "Package boot asset contents changed during install: {}",
            path.display()
        );
    }
    Ok(())
}

fn resolve_boot_asset(boot_root: &Path, asset: &Path) -> Result<PathBuf> {
    let path = boot_root.join(asset);
    let canonical = fs::canonicalize(&path)
        .with_context(|| format!("Canonicalizing package boot asset {}", path.display()))?;
    ensure!(
        canonical.starts_with(boot_root),
        "Package boot asset resolves outside /boot: {} -> {}",
        path.display(),
        canonical.display()
    );
    ensure!(
        fs::metadata(&canonical)
            .with_context(|| format!("Inspecting package boot asset {}", canonical.display()))?
            .is_file(),
        "Package boot asset does not resolve to a regular file: {}",
        path.display()
    );
    Ok(canonical)
}

fn ensure_unique_title(entries_path: &Path, title: &str) -> Result<()> {
    let mut matching_titles = 0;
    for entry in fs::read_dir(entries_path)
        .with_context(|| format!("Reading BLS entries in {}", entries_path.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("conf") {
            continue;
        }
        ensure!(
            fs::symlink_metadata(&path)
                .with_context(|| format!("Inspecting BLS entry {}", path.display()))?
                .is_file(),
            "BLS entry is not a regular file: {}",
            path.display()
        );
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("Reading BLS entry {}", path.display()))?;
        let config = parse_bls_config(&contents)
            .with_context(|| format!("Parsing BLS entry {}", path.display()))?;
        if config.title.as_deref() == Some(title) {
            matching_titles += 1;
        }
    }
    ensure!(
        matching_titles == 1,
        "Package-mode BLS title is not unique: {title:?}"
    );
    Ok(())
}

fn hash_file(path: &Path) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("Opening package boot asset {}", path.display()))?;
    let mut hasher = openssl::hash::Hasher::new(openssl::hash::MessageDigest::sha256())?;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("Hashing package boot asset {}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count])?;
    }
    Ok(hasher.finish()?.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path) -> Result<()> {
        let boot = root.join("boot");
        fs::create_dir_all(boot.join("loader/entries"))?;
        fs::create_dir_all(boot.join("grub2"))?;
        fs::write(boot.join("grub2/grub.cfg"), "blscfg\n")?;
        fs::write(boot.join("vmlinuz-6.9.0"), b"kernel")?;
        fs::write(boot.join("initramfs-6.9.0.img"), b"initrd")?;
        fs::write(boot.join("devicetree-6.9.0.dtb"), b"devicetree")?;
        fs::write(
            boot.join("loader/entries/pkg-6.9.0.conf"),
            "title Package OS\nversion 6.9.0\nlinux /vmlinuz-6.9.0\ninitrd /initramfs-6.9.0.img\ndevicetree /devicetree-6.9.0.dtb\noptions root=UUID=old-root ro quiet\n",
        )?;
        Ok(())
    }

    fn running_cmdline() -> &'static str {
        "BOOT_IMAGE=(hd0,gpt2)/vmlinuz-6.9.0 root=UUID=old-root ro quiet"
    }

    #[test]
    fn validates_and_rechecks_existing_entry_assets() -> Result<()> {
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        let entry = PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline())?;
        entry.verify_retained()?;

        fs::write(
            root.path().join("boot/vmlinuz-6.9.0"),
            b"replacement kernel",
        )?;
        assert!(entry.verify_retained().is_err());
        Ok(())
    }

    #[test]
    fn rejects_missing_assets_and_non_package_command_lines() -> Result<()> {
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        fs::remove_file(root.path().join("boot/initramfs-6.9.0.img"))?;
        assert!(PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline()).is_err());
        assert!(RunningCmdline::parse("root=UUID=x ostree=/ostree/boot.0").is_err());
        assert!(RunningCmdline::parse("quiet").is_err());
        Ok(())
    }

    #[test]
    fn rejects_malformed_existing_bls_entry_even_with_a_valid_match() -> Result<()> {
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        fs::write(
            root.path()
                .join("boot/loader/entries/unrelated-broken.conf"),
            "this is not a valid BLS entry\n",
        )?;
        let error = PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline())
            .unwrap_err()
            .to_string();
        assert!(error.contains("Parsing BLS entry"));
        Ok(())
    }

    #[test]
    fn rejects_duplicate_running_title() -> Result<()> {
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        fs::write(
            root.path().join("boot/loader/entries/old-kernel.conf"),
            "title Package OS\nversion 6.8.0\nlinux /vmlinuz-6.8.0\n",
        )?;

        let error = PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline())
            .unwrap_err()
            .to_string();
        assert!(error.contains("BLS title is not unique"));
        Ok(())
    }

    #[test]
    fn rejects_asset_symlink_escape_from_boot_root() -> Result<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        let outside = tempfile::tempdir()?;
        fs::write(
            outside.path().join("initramfs-6.9.0.img"),
            b"outside initrd",
        )?;
        let boot = root.path().join("boot");
        fs::remove_file(boot.join("initramfs-6.9.0.img"))?;
        symlink(outside.path(), boot.join("external"))?;
        fs::write(
            boot.join("loader/entries/pkg-6.9.0.conf"),
            "title Package OS\nversion 6.9.0\nlinux /vmlinuz-6.9.0\ninitrd /boot/external/initramfs-6.9.0.img\ndevicetree /devicetree-6.9.0.dtb\noptions root=UUID=old-root ro quiet\n",
        )?;

        let error = PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline())
            .unwrap_err()
            .to_string();
        assert!(error.contains("resolves outside /boot"));
        Ok(())
    }

    #[test]
    fn rejects_empty_running_entry_title() -> Result<()> {
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        fs::write(
            root.path().join("boot/loader/entries/pkg-6.9.0.conf"),
            "title \nversion 6.9.0\nlinux /vmlinuz-6.9.0\ninitrd /initramfs-6.9.0.img\noptions root=UUID=old-root ro quiet\n",
        )?;

        let error = PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline())
            .unwrap_err()
            .to_string();
        assert!(error.contains("no non-empty title"));
        Ok(())
    }

    #[test]
    fn rejects_grub_variables_in_referenced_asset_paths() -> Result<()> {
        let cases = [
            ("linux /vmlinuz-6.9.0", "linux $root/vmlinuz-6.9.0"),
            (
                "initrd /initramfs-6.9.0.img",
                "initrd $prefix/initramfs-6.9.0.img",
            ),
            (
                "devicetree /devicetree-6.9.0.dtb",
                "devicetree ${bootroot}/devicetree-6.9.0.dtb",
            ),
        ];
        for (original, expanded) in cases {
            let root = tempfile::tempdir()?;
            fixture(root.path())?;
            let entry = root.path().join("boot/loader/entries/pkg-6.9.0.conf");
            let contents = fs::read_to_string(&entry)?;
            fs::write(&entry, contents.replace(original, expanded))?;

            let error = PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline())
                .unwrap_err()
                .to_string();
            assert!(error.contains("GRUB variable expansion"), "{error}");
        }
        Ok(())
    }

    #[test]
    fn rejects_duplicate_singleton_bls_directives() -> Result<()> {
        for directive in ["linux", "version", "options", "architecture"] {
            let contents = format!("{directive} first\n{directive} second\n");
            let error = validate_unique_singleton_directives(&contents)
                .unwrap_err()
                .to_string();
            assert!(error.contains(&format!("Duplicate BLS {directive} directive")));
        }
        // `initrd` is intentionally repeatable in BLS and represents an
        // ordered list, not an ambiguous singleton directive.
        validate_unique_singleton_directives("initrd /intel-ucode.img\ninitrd /initramfs.img\n")?;
        Ok(())
    }

    #[test]
    fn rejects_both_efi_and_uki_aliases() -> Result<()> {
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        let entry = root.path().join("boot/loader/entries/pkg-6.9.0.conf");
        let contents = fs::read_to_string(&entry)?;
        fs::write(
            &entry,
            format!("{contents}efi /EFI/Linux/package.efi\nuki /EFI/Linux/package.efi\n"),
        )?;

        let error =
            PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline()).unwrap_err();
        assert!(format!("{error:#}").contains("both 'efi' and 'uki'"));
        Ok(())
    }

    #[test]
    fn accepts_multiple_ordered_initrd_assets() -> Result<()> {
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        let boot = root.path().join("boot");
        fs::write(boot.join("intel-ucode.img"), b"microcode")?;
        let entry = boot.join("loader/entries/pkg-6.9.0.conf");
        let contents = fs::read_to_string(&entry)?;
        fs::write(
            &entry,
            contents.replace(
                "initrd /initramfs-6.9.0.img\n",
                "initrd /intel-ucode.img\ninitrd /initramfs-6.9.0.img\n",
            ),
        )?;
        PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline())?;
        Ok(())
    }

    #[test]
    fn normalizes_boot_mountpoint_conventions_and_retains_assets() -> Result<()> {
        for (bls_prefix, image_prefix) in [("/boot", ""), ("", "/boot")] {
            let root = tempfile::tempdir()?;
            fixture(root.path())?;
            let boot = root.path().join("boot");
            fs::write(boot.join("intel-ucode.img"), b"microcode")?;
            let entry_path = boot.join("loader/entries/pkg-6.9.0.conf");
            let contents = fs::read_to_string(&entry_path)?;
            let contents = contents
                .replace("/vmlinuz-6.9.0", &format!("{bls_prefix}/vmlinuz-6.9.0"))
                .replace(
                    "/initramfs-6.9.0.img",
                    &format!("{bls_prefix}/initramfs-6.9.0.img"),
                )
                .replace(
                    "/devicetree-6.9.0.dtb",
                    &format!("{bls_prefix}/devicetree-6.9.0.dtb"),
                );
            let contents = contents.replace(
                &format!("initrd {bls_prefix}/initramfs-6.9.0.img\n"),
                &format!(
                    "initrd {bls_prefix}/intel-ucode.img\ninitrd {bls_prefix}/initramfs-6.9.0.img\n"
                ),
            );
            fs::write(&entry_path, contents)?;

            let cmdline = format!(
                "BOOT_IMAGE=(hd0,gpt2){image_prefix}/vmlinuz-6.9.0 root=UUID=old-root ro quiet"
            );
            let entry = PackageBootEntry::preflight(root.path(), "6.9.0", &cmdline)?;
            entry.verify_retained()?;

            fs::write(boot.join("intel-ucode.img"), b"changed microcode")?;
            let error = entry.verify_retained().unwrap_err().to_string();
            assert!(error.contains("intel-ucode.img"), "{error}");
        }
        Ok(())
    }

    #[test]
    fn normalizes_only_exact_boot_mountpoint_prefixes() -> Result<()> {
        for (input, expected) in [
            ("/boot/vmlinuz", "vmlinuz"),
            ("/vmlinuz", "vmlinuz"),
            ("/booted/vmlinuz", "booted/vmlinuz"),
        ] {
            assert_eq!(normalize_boot_path(input)?, PathBuf::from(expected));
        }

        let error = normalize_boot_path("/boot/../outside").unwrap_err();
        assert!(error.to_string().contains("Unsafe BLS boot asset path"));
        Ok(())
    }

    #[test]
    fn postflight_rejects_changed_initrd_and_devicetree() -> Result<()> {
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        let entry = PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline())?;
        fs::write(
            root.path().join("boot/initramfs-6.9.0.img"),
            b"replacement initrd",
        )?;
        let error = entry.verify_retained().unwrap_err().to_string();
        assert!(error.contains("initramfs-6.9.0.img"));

        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        let entry = PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline())?;
        fs::write(
            root.path().join("boot/devicetree-6.9.0.dtb"),
            b"replacement devicetree",
        )?;
        let error = entry.verify_retained().unwrap_err().to_string();
        assert!(error.contains("devicetree-6.9.0.dtb"));
        Ok(())
    }

    #[test]
    fn postflight_rejects_bls_entry_mutation() -> Result<()> {
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        let entry = PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline())?;
        let path = root.path().join("boot/loader/entries/pkg-6.9.0.conf");
        let contents = fs::read_to_string(&path)?;
        fs::write(
            &path,
            contents.replace("title Package OS", "title Changed OS"),
        )?;

        let error = entry.verify_retained().unwrap_err().to_string();
        assert!(error.contains("BLS entry changed or disappeared"));
        Ok(())
    }

    #[test]
    fn postflight_rejects_a_new_duplicate_title() -> Result<()> {
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        let entry = PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline())?;
        fs::write(
            root.path().join("boot/loader/entries/new-entry.conf"),
            "title Package OS\nversion 6.8.0\nlinux /vmlinuz-6.8.0\n",
        )?;

        let error = entry.verify_retained().unwrap_err().to_string();
        assert!(error.contains("BLS title is not unique"));
        Ok(())
    }

    #[test]
    fn rejects_entry_for_a_different_root() -> Result<()> {
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        fs::write(
            root.path().join("boot/loader/entries/pkg-6.9.0.conf"),
            "title Package OS\nversion 6.9.0\nlinux /vmlinuz-6.9.0\ninitrd /initramfs-6.9.0.img\noptions root=UUID=other-root ro quiet\n",
        )?;
        assert!(
            PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline())
                .unwrap_err()
                .to_string()
                .contains("does not match running root")
        );
        Ok(())
    }

    #[test]
    fn rejects_ambiguous_same_version_entries() -> Result<()> {
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        fs::copy(
            root.path().join("boot/loader/entries/pkg-6.9.0.conf"),
            root.path().join("boot/loader/entries/duplicate-6.9.0.conf"),
        )?;
        assert!(
            PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline())
                .unwrap_err()
                .to_string()
                .contains("found 2")
        );
        Ok(())
    }

    #[test]
    fn ignores_unrelated_same_version_uki_entry() -> Result<()> {
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        fs::write(
            root.path()
                .join("boot/loader/entries/unrelated-uki-6.9.0.conf"),
            "title Unrelated UKI\nversion 6.9.0\nuki /EFI/Linux/unrelated.efi\n",
        )?;

        PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline())?;
        Ok(())
    }

    #[test]
    fn rejects_matching_uki_entry() -> Result<()> {
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        fs::write(
            root.path()
                .join("boot/loader/entries/package-uki-6.9.0.conf"),
            "title Package UKI\nversion 6.9.0\nuki /vmlinuz-6.9.0\n",
        )?;

        let error = PackageBootEntry::preflight(root.path(), "6.9.0", running_cmdline())
            .unwrap_err()
            .to_string();
        assert!(error.contains("selected running-kernel BLS entry is an EFI/UKI"));
        Ok(())
    }

    #[test]
    fn rejects_running_options_mismatch() -> Result<()> {
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        let error = PackageBootEntry::preflight(
            root.path(),
            "6.9.0",
            "BOOT_IMAGE=(hd0,gpt2)/vmlinuz-6.9.0 root=UUID=old-root ro quiet splash",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("options do not match /proc/cmdline"));
        Ok(())
    }

    #[test]
    fn rejects_entry_for_a_different_running_kernel_image() -> Result<()> {
        let root = tempfile::tempdir()?;
        fixture(root.path())?;
        assert!(
            PackageBootEntry::preflight(
                root.path(),
                "6.9.0",
                "BOOT_IMAGE=(hd0,gpt2)/vmlinuz-other root=UUID=old-root ro quiet"
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn resolves_only_explicit_kernelopts_and_rejects_other_dynamic_variables() {
        assert!(resolve_options("$kernelopts", None).is_err());
        assert_eq!(
            resolve_options("$kernelopts", Some("root=UUID=old-root ro quiet")).unwrap(),
            "root=UUID=old-root ro quiet"
        );
        assert!(resolve_options("$tuned_params", Some("quiet")).is_err());
        assert!(resolve_options("$kernelopts $tuned_params", Some("quiet")).is_err());
        assert_eq!(
            parse_kernelopts("saved_entry=0\nkernelopts=root=UUID=old-root ro quiet\n").unwrap(),
            "root=UUID=old-root ro quiet"
        );
        assert!(parse_kernelopts("saved_entry=0\n").is_err());
        assert!(parse_kernelopts("kernelopts=one\nkernelopts=two\n").is_err());
    }
}
