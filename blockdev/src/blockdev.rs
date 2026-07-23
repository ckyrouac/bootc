use std::collections::{HashMap, HashSet};
use std::env;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{anyhow, Context, Result};
use camino::Utf8Path;
use camino::Utf8PathBuf;
use fn_error_context::context;
use regex::Regex;
use serde::Deserialize;

use bootc_utils::CommandRunExt;

/// MBR partition type IDs that indicate an EFI System Partition.
/// 0x06 is FAT16 (used as ESP on some MBR systems), 0xEF is the
/// explicit EFI System Partition type.
/// Refer to <https://en.wikipedia.org/wiki/Partition_type>
pub const ESP_ID_MBR: &[u8] = &[0x06, 0xEF];

/// EFI System Partition (ESP) for UEFI boot on GPT
pub const ESP: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";

/// BIOS boot partition type GUID for GPT
pub const BIOS_BOOT: &str = "21686148-6449-6e6f-744e-656564454649";

#[derive(Debug, Deserialize)]
struct DevicesOutput {
    blockdevices: Vec<Device>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct Device {
    pub name: String,
    pub serial: Option<String>,
    pub model: Option<String>,
    pub partlabel: Option<String>,
    pub parttype: Option<String>,
    pub partuuid: Option<String>,
    /// Partition number (1-indexed). None for whole disk devices.
    pub partn: Option<u32>,
    pub children: Option<Vec<Device>>,
    pub size: u64,
    #[serde(rename = "maj:min")]
    pub maj_min: Option<String>,
    // NOTE this one is not available on older util-linux, and
    // will also not exist for whole blockdevs (as opposed to partitions).
    pub start: Option<u64>,

    // Filesystem-related properties
    pub label: Option<String>,
    pub fstype: Option<String>,
    pub uuid: Option<String>,
    pub path: Option<String>,
    /// Partition table type (e.g., "gpt", "dos"). Only present on whole disk devices.
    pub pttype: Option<String>,
}

impl Device {
    #[allow(dead_code)]
    // RHEL8's lsblk doesn't have PATH, so we do it
    pub fn path(&self) -> String {
        self.path.clone().unwrap_or(format!("/dev/{}", &self.name))
    }

    #[allow(dead_code)]
    pub fn has_children(&self) -> bool {
        self.children.as_ref().map_or(false, |v| !v.is_empty())
    }

    // Check if the device is mpath
    pub fn is_mpath(&self) -> Result<bool> {
        let dm_path = Utf8PathBuf::from_path_buf(std::fs::canonicalize(self.path())?)
            .map_err(|_| anyhow::anyhow!("Non-UTF8 path"))?;
        let dm_name = dm_path.file_name().unwrap_or("");
        let uuid_path = Utf8PathBuf::from(format!("/sys/class/block/{dm_name}/dm/uuid"));

        if uuid_path.exists() {
            let uuid = std::fs::read_to_string(&uuid_path)
                .with_context(|| format!("Failed to read {uuid_path}"))?;
            if uuid.trim_start().starts_with("mpath-") {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Read a sysfs property for this device and parse it as the target type.
    fn read_sysfs_property<T>(&self, property: &str) -> Result<Option<T>>
    where
        T: std::str::FromStr,
        T::Err: std::error::Error + Send + Sync + 'static,
    {
        let Some(majmin) = self.maj_min.as_deref() else {
            return Ok(None);
        };
        let sysfs_path = format!("/sys/dev/block/{majmin}/{property}");
        if !Utf8Path::new(&sysfs_path).try_exists()? {
            return Ok(None);
        }
        let value = std::fs::read_to_string(&sysfs_path)
            .with_context(|| format!("Reading {sysfs_path}"))?;
        let parsed = value
            .trim()
            .parse()
            .with_context(|| format!("Parsing sysfs {property} property"))?;
        tracing::debug!("backfilled {property} to {value}");
        Ok(Some(parsed))
    }

    // The "start" parameter was only added in a version of util-linux that's only
    // in Fedora 40 as of this writing.
    fn backfill_start(&mut self) -> Result<()> {
        if self.start.is_none() {
            self.start = self.read_sysfs_property("start")?;
        }
        Ok(())
    }

    /// Backfill the "partn" field from sysfs when lsblk doesn't provide it.
    /// util-linux 2.39+ provides partn; RHEL 9 ships 2.37 so we fall back.
    fn backfill_partn(&mut self) -> Result<()> {
        if self.partn.is_none() {
            // Note: sysfs uses "partition" not "partn"
            self.partn = self.read_sysfs_property("partition")?;
        }
        Ok(())
    }

    /// Older versions of util-linux may be missing some properties. Backfill them if they're missing.
    pub fn backfill_missing(&mut self) -> Result<()> {
        // Add new properties to backfill here
        self.backfill_start()?;
        self.backfill_partn()?;
        // And recurse to child devices
        for child in self.children.iter_mut().flatten() {
            child.backfill_missing()?;
        }
        Ok(())
    }

    /// Find a child partition by partition type (case-insensitive).
    pub fn find_partition_of_type(&self, parttype: &str) -> Option<&Device> {
        self.children.as_ref()?.iter().find(|child| {
            child
                .parttype
                .as_ref()
                .map_or(false, |pt| pt.eq_ignore_ascii_case(parttype))
        })
    }

    /// Find the EFI System Partition (ESP) among children.
    ///
    /// For GPT disks, this matches by the ESP partition type GUID.
    /// For MBR (dos) disks, this matches by the MBR partition type IDs (0x06 or 0xEF).
    ///
    /// If no ESP is found among direct children, this recurses into children
    /// that have their own partition table (e.g. firmware RAID arrays where the
    /// hierarchy is disk → md array → partitions).
    ///
    /// Returns `Ok(None)` when there are no children or no ESP partition
    /// is present. Returns `Err` only for genuinely unexpected conditions
    /// (e.g. an unsupported partition table type).
    pub fn find_partition_of_esp_optional(&self) -> Result<Option<&Device>> {
        let Some(children) = self.children.as_ref() else {
            return Ok(None);
        };
        let direct = match self.pttype.as_deref() {
            Some("dos") => children.iter().find(|child| {
                child
                    .parttype
                    .as_ref()
                    .and_then(|pt| {
                        let pt = pt.strip_prefix("0x").unwrap_or(pt);
                        u8::from_str_radix(pt, 16).ok()
                    })
                    .map_or(false, |pt| ESP_ID_MBR.contains(&pt))
            }),
            // When pttype is None (e.g. older lsblk or partition devices), default
            // to GPT UUID matching which will simply not match MBR hex types.
            Some("gpt") | None => self.find_partition_of_type(ESP),
            Some(other) => return Err(anyhow!("Unsupported partition table type: {other}")),
        };
        if direct.is_some() {
            return Ok(direct);
        }
        // Recurse into children that carry their own partition table, such as
        // firmware RAID arrays (disk → md array → partitions).
        for child in children {
            if child.pttype.is_some() {
                if let Some(esp) = child.find_partition_of_esp_optional()? {
                    return Ok(Some(esp));
                }
            }
        }
        Ok(None)
    }

    /// Find the EFI System Partition (ESP) among children, or error if absent.
    pub fn find_partition_of_esp(&self) -> Result<&Device> {
        self.find_partition_of_esp_optional()?
            .ok_or_else(|| anyhow!("ESP partition not found on {}", self.path()))
    }

    /// Find BIOS boot partition among children.
    pub fn find_partition_of_bios_boot(&self) -> Option<&Device> {
        self.find_partition_of_type(BIOS_BOOT)
    }

    /// Find a child partition by partition number (1-indexed).
    pub fn find_device_by_partno(&self, partno: u32) -> Result<&Device> {
        self.children
            .as_ref()
            .ok_or_else(|| anyhow!("Device has no children"))?
            .iter()
            .find(|child| child.partn == Some(partno))
            .ok_or_else(|| anyhow!("Missing partition for index {partno}"))
    }

    /// Re-query this device's information from lsblk, updating all fields.
    pub fn refresh(&mut self) -> Result<()> {
        let path = self.path();
        let new_device = list_dev(Utf8Path::new(&path))?;
        *self = new_device;
        Ok(())
    }

    /// Get the numeric partition index of the ESP (e.g. "1", "2").
    ///
    /// We read `/sys/class/block/<name>/partition` rather than parsing device
    /// names because naming conventions vary across disk types (sd, nvme, dm, etc.).
    /// On multipath devices the sysfs `partition` attribute doesn't exist, so we
    /// fall back to the `partn` field reported by lsblk, then to parsing the
    /// partition suffix from the ESP device path relative to the parent device
    /// path (e.g. parent `/dev/mapper/mpatha`, ESP `/dev/mapper/mpatha2` → `"2"`).
    pub fn get_esp_partition_number(&self) -> Result<String> {
        let esp_device = self.find_partition_of_esp()?;
        let devname = &esp_device.name;

        let partition_path = Utf8PathBuf::from(format!("/sys/class/block/{devname}/partition"));
        if partition_path.exists() {
            return std::fs::read_to_string(&partition_path)
                .with_context(|| format!("Failed to read {partition_path}"));
        }

        // On multipath the partition attribute is not existing
        if self.is_mpath()? {
            if let Some(partn) = esp_device.partn {
                return Ok(partn.to_string());
            }
            // Last resort: strip the parent device path from the ESP device path,
            // then skip any non-digit separator (e.g. "p") to get the partition number.
            let parent_path = self.path();
            let esp_path = esp_device.path();
            if let Some(n) = parse_partition_number_from_suffix(&parent_path, &esp_path) {
                return Ok(n);
            }
        }
        anyhow::bail!("Not supported for {devname}")
    }

    /// Query parent devices via `lsblk --inverse`.
    ///
    /// Returns `Ok(None)` if this device is already a root device (no parents).
    pub fn list_parents(&self) -> Result<Option<Vec<Device>>> {
        let path = self.path();
        let output: DevicesOutput = Command::new("lsblk")
            .args(["-J", "-b", "-O", "--inverse"])
            .arg(&path)
            .log_debug()
            .run_and_parse_json()?;

        let device = output
            .blockdevices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no device output from lsblk --inverse for {path}"))?;

        match device.children {
            Some(mut children) if !children.is_empty() => {
                for child in &mut children {
                    child.backfill_missing()?;
                }
                Ok(Some(children))
            }
            _ => Ok(None),
        }
    }

    /// Walk the parent chain to find all root (whole disk) devices,
    /// and fail if more than one root is found.
    ///
    /// This is a convenience wrapper around `find_all_roots` for callers
    /// that expect exactly one backing device (e.g. non-RAID setups).
    pub fn require_single_root(&self) -> Result<Device> {
        let mut roots = self.find_all_roots()?;
        match roots.len() {
            1 => Ok(roots.remove(0)),
            n => anyhow::bail!(
                "Expected a single root device for {}, but found {n}",
                self.path()
            ),
        }
    }

    /// Walk the parent chain to find all root (whole disk) devices.
    ///
    /// Returns all root devices with their children (partitions) populated.
    /// This handles devices backed by multiple parents (e.g. RAID arrays)
    /// by following all branches of the parent tree.
    /// If this device is already a root device, returns a single-element list.
    pub fn find_all_roots(&self) -> Result<Vec<Device>> {
        let Some(parents) = self.list_parents()? else {
            // Already a root device; re-query to ensure children are populated
            return Ok(vec![list_dev(Utf8Path::new(&self.path()))?]);
        };

        let mut roots = Vec::new();
        let mut seen = HashSet::new();
        let mut queue = parents;
        while let Some(mut device) = queue.pop() {
            match device.children.take() {
                Some(grandparents) if !grandparents.is_empty() => {
                    queue.extend(grandparents);
                }
                _ => {
                    // Deduplicate: in complex topologies (e.g. multipath)
                    // multiple branches can converge on the same physical disk.
                    let name = device.name.clone();
                    if seen.insert(name) {
                        // Found a new root; re-query to populate its actual children
                        roots.push(list_dev(Utf8Path::new(&device.path()))?);
                    }
                }
            }
        }
        Ok(roots)
    }

    /// Find all ESP partitions across all root devices backing this device.
    /// Returns None if no ESPs are found.
    pub fn find_colocated_esps(&self) -> Result<Option<Vec<Device>>> {
        let mut esps = Vec::new();
        for root in &self.find_all_roots()? {
            if let Some(esp) = root.find_partition_of_esp_optional()? {
                esps.push(esp.clone());
            }
        }
        Ok((!esps.is_empty()).then_some(esps))
    }

    /// Find a single ESP partition among all root devices backing this device.
    ///
    /// Returns the first ESP found. This is the common case for boot paths
    /// where exactly one ESP is expected.
    pub fn find_first_colocated_esp(&self) -> Result<Device> {
        self.find_colocated_esps()?
            .and_then(|mut v| if v.is_empty() { None } else { Some(v.remove(0)) })
            .ok_or_else(|| anyhow!("No ESP partition found among backing devices"))
    }
}

#[context("Listing device {dev}")]
pub fn list_dev(dev: &Utf8Path) -> Result<Device> {
    let mut devs: DevicesOutput = Command::new("lsblk")
        .args(["-J", "-b", "-O"])
        .arg(dev)
        .log_debug()
        .run_and_parse_json()?;
    for dev in devs.blockdevices.iter_mut() {
        dev.backfill_missing()?;
    }
    devs.blockdevices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no device output from lsblk for {dev}"))
}

#[derive(Debug, Deserialize)]
struct SfDiskOutput {
    partitiontable: PartitionTable,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Partition {
    pub node: String,
    pub start: u64,
    pub size: u64,
    #[serde(rename = "type")]
    pub parttype: String,
    pub uuid: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PartitionType {
    Dos,
    Gpt,
    Unknown(String),
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct PartitionTable {
    pub label: PartitionType,
    pub id: String,
    pub device: String,
    // We're not using these fields
    // pub unit: String,
    // pub firstlba: u64,
    // pub lastlba: u64,
    // pub sectorsize: u64,
    pub partitions: Vec<Partition>,
}

impl PartitionTable {
    /// Find the partition with the given device name
    #[allow(dead_code)]
    pub fn find<'a>(&'a self, devname: &str) -> Option<&'a Partition> {
        self.partitions.iter().find(|p| p.node.as_str() == devname)
    }

    pub fn path(&self) -> &Utf8Path {
        self.device.as_str().into()
    }

    // Find the partition with the given offset (starting at 1)
    #[allow(dead_code)]
    pub fn find_partno(&self, partno: u32) -> Result<&Partition> {
        let r = self
            .partitions
            .get(partno.checked_sub(1).expect("1 based partition offset") as usize)
            .ok_or_else(|| anyhow::anyhow!("Missing partition for index {partno}"))?;
        Ok(r)
    }
}

impl Partition {
    #[allow(dead_code)]
    pub fn path(&self) -> &Utf8Path {
        self.node.as_str().into()
    }
}

#[context("Listing partitions of {dev}")]
pub fn partitions_of(dev: &Utf8Path) -> Result<PartitionTable> {
    let o: SfDiskOutput = Command::new("sfdisk")
        .args(["-J", dev.as_str()])
        .run_and_parse_json()?;
    Ok(o.partitiontable)
}

pub struct LoopbackDevice {
    pub dev: Option<Utf8PathBuf>,
}

impl LoopbackDevice {
    // Create a new loopback block device targeting the provided file path.
    pub fn new(path: &Path) -> Result<Self> {
        let direct_io = match env::var("BOOTC_DIRECT_IO") {
            Ok(val) => {
                if val == "on" {
                    "on"
                } else {
                    "off"
                }
            }
            Err(_e) => "off",
        };

        let dev = Command::new("losetup")
            .args([
                "--show",
                format!("--direct-io={direct_io}").as_str(),
                "-P",
                "--find",
            ])
            .arg(path)
            .run_get_string()?;
        let dev = Utf8PathBuf::from(dev.trim());
        tracing::debug!("Allocated loopback {dev}");
        Ok(Self { dev: Some(dev) })
    }

    // Access the path to the loopback block device.
    pub fn path(&self) -> &Utf8Path {
        // SAFETY: The option cannot be destructured until we are dropped
        self.dev.as_deref().unwrap()
    }

    // Shared backend for our `close` and `drop` implementations.
    fn impl_close(&mut self) -> Result<()> {
        // SAFETY: This is the only place we take the option
        let Some(dev) = self.dev.take() else {
            tracing::trace!("loopback device already deallocated");
            return Ok(());
        };
        Command::new("losetup").args(["-d", dev.as_str()]).run()
    }

    /// Consume this device, unmounting it.
    pub fn close(mut self) -> Result<()> {
        self.impl_close()
    }
}

impl Drop for LoopbackDevice {
    fn drop(&mut self) {
        // Best effort to unmount if we're dropped without invoking `close`
        let _ = self.impl_close();
    }
}

/// Parse key-value pairs from lsblk --pairs.
/// Newer versions of lsblk support JSON but the one in CentOS 7 doesn't.
fn split_lsblk_line(line: &str) -> HashMap<String, String> {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    let regex = REGEX.get_or_init(|| Regex::new(r#"([A-Z-_]+)="([^"]+)""#).unwrap());
    let mut fields: HashMap<String, String> = HashMap::new();
    for cap in regex.captures_iter(line) {
        fields.insert(cap[1].to_string(), cap[2].to_string());
    }
    fields
}

/// This is a bit fuzzy, but... this function will return every block device in the parent
/// hierarchy of `device` capable of containing other partitions. So e.g. parent devices of type
/// "part" doesn't match, but "disk" and "mpath" does.
pub fn find_parent_devices(device: &str) -> Result<Vec<String>> {
    let output = Command::new("lsblk")
        // Older lsblk, e.g. in CentOS 7.6, doesn't support PATH, but --paths option
        .arg("--pairs")
        .arg("--paths")
        .arg("--inverse")
        .arg("--output")
        .arg("NAME,TYPE")
        .arg(device)
        .run_get_string()?;
    let mut parents = Vec::new();
    // skip first line, which is the device itself
    for line in output.lines().skip(1) {
        let dev = split_lsblk_line(line);
        let name = dev
            .get("NAME")
            .with_context(|| format!("device in hierarchy of {device} missing NAME"))?;
        let kind = dev
            .get("TYPE")
            .with_context(|| format!("device in hierarchy of {device} missing TYPE"))?;
        if kind == "disk" || kind == "loop" {
            parents.push(name.clone());
        } else if kind == "mpath" {
            parents.push(name.clone());
            // we don't need to know what disks back the multipath
            break;
        }
    }
    Ok(parents)
}

/// Extract a partition number by stripping the parent device path from the
/// ESP partition device path, then skipping any non-digit separator characters.
///
/// Multipath partition devices are named by appending a partition suffix to
/// the parent device path. The suffix may include a separator like "p" before
/// the digits:
///   - `/dev/mapper/mpatha`  + `2`  → `/dev/mapper/mpatha2`
///   - `/dev/mapper/mpatha`  + `p2` → `/dev/mapper/mpathap2`
///
/// This function returns `None` if the ESP path doesn't start with the parent
/// path or if no trailing digits are found in the suffix.
fn parse_partition_number_from_suffix(parent_path: &str, esp_path: &str) -> Option<String> {
    let suffix = esp_path.strip_prefix(parent_path)?;
    let digits = suffix.trim_start_matches(|c: char| !c.is_ascii_digit());
    if digits.is_empty() {
        return None;
    }
    Some(digits.to_string())
}

/// Parse a string into mibibytes
pub fn parse_size_mib(mut s: &str) -> Result<u64> {
    let suffixes = [
        ("MiB", 1u64),
        ("M", 1u64),
        ("GiB", 1024),
        ("G", 1024),
        ("TiB", 1024 * 1024),
        ("T", 1024 * 1024),
    ];
    let mut mul = 1u64;
    for (suffix, imul) in suffixes {
        if let Some((sv, rest)) = s.rsplit_once(suffix) {
            if !rest.is_empty() {
                anyhow::bail!("Trailing text after size: {rest}");
            }
            s = sv;
            mul = imul;
        }
    }
    let v = s.parse::<u64>()?;
    Ok(v * mul)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_parse_size_mib() {
        let ident_cases = [0, 10, 9, 1024].into_iter().map(|k| (k.to_string(), k));
        let cases = [
            ("0M", 0),
            ("10M", 10),
            ("10MiB", 10),
            ("1G", 1024),
            ("9G", 9216),
            ("11T", 11 * 1024 * 1024),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v));
        for (s, v) in ident_cases.chain(cases) {
            assert_eq!(parse_size_mib(&s).unwrap(), v as u64, "Parsing {s}");
        }
    }

    #[test]
    fn test_parse_lsblk() {
        let fixture = include_str!("../tests/fixtures/lsblk.json");
        let devs: DevicesOutput = serde_json::from_str(&fixture).unwrap();
        let dev = devs.blockdevices.into_iter().next().unwrap();
        // The parent device has no partition number
        assert_eq!(dev.partn, None);
        let children = dev.children.as_deref().unwrap();
        assert_eq!(children.len(), 3);
        let first_child = &children[0];
        assert_eq!(first_child.partn, Some(1));
        assert_eq!(
            first_child.parttype.as_deref().unwrap(),
            "21686148-6449-6e6f-744e-656564454649"
        );
        assert_eq!(
            first_child.partuuid.as_deref().unwrap(),
            "3979e399-262f-4666-aabc-7ab5d3add2f0"
        );
    }

    #[test]
    fn test_parse_lsblk_vroc() {
        let fixture = include_str!("../tests/fixtures/lsblk-vroc.json");
        let devs: DevicesOutput = serde_json::from_str(fixture).unwrap();
        assert_eq!(devs.blockdevices.len(), 2);

        // find_partition_of_esp recurses through the md126 RAID array to
        // locate the ESP (md126p1) even though it is not a direct child of
        // the NVMe disk.
        for nvme in &devs.blockdevices {
            let esp = nvme.find_partition_of_esp().unwrap();
            assert_eq!(esp.name, "md126p1");
            assert_eq!(esp.partn, Some(1));
            assert_eq!(esp.parttype.as_deref().unwrap(), ESP);
            assert_eq!(esp.fstype.as_deref().unwrap(), "vfat");
        }
    }

    #[test]
    fn test_parse_lsblk_swraid() {
        let fixture = include_str!("../tests/fixtures/lsblk-swraid.json");
        let devs: DevicesOutput = serde_json::from_str(fixture).unwrap();
        assert_eq!(devs.blockdevices.len(), 2);

        // In a software RAID (mdadm) setup each disk is individually
        // partitioned with its own GPT table and ESP. The root partition
        // (sda3/sdb3) is a linux_raid_member assembled into md0.
        // find_partition_of_esp should locate the ESP as a direct child of
        // each disk — no recursion through an md array is needed here.
        let sda = &devs.blockdevices[0];
        let esp = sda.find_partition_of_esp().unwrap();
        assert_eq!(esp.name, "sda1");
        assert_eq!(esp.partn, Some(1));
        assert_eq!(esp.parttype.as_deref().unwrap(), ESP);
        assert_eq!(esp.fstype.as_deref().unwrap(), "vfat");

        let sdb = &devs.blockdevices[1];
        let esp = sdb.find_partition_of_esp().unwrap();
        assert_eq!(esp.name, "sdb1");
        assert_eq!(esp.partn, Some(1));
        assert_eq!(esp.parttype.as_deref().unwrap(), ESP);
        assert_eq!(esp.fstype.as_deref().unwrap(), "vfat");

        // Verify the md0 RAID array is visible as a child of the root
        // partition on each disk.
        let sda3 = sda
            .children
            .as_ref()
            .unwrap()
            .iter()
            .find(|c| c.name == "sda3")
            .unwrap();
        assert_eq!(sda3.fstype.as_deref().unwrap(), "linux_raid_member");
        let md0 = sda3
            .children
            .as_ref()
            .unwrap()
            .iter()
            .find(|c| c.name == "md0")
            .unwrap();
        assert_eq!(md0.fstype.as_deref().unwrap(), "ext4");
    }

    /// Helper to construct a minimal MBR disk Device with given child partition types.
    fn make_mbr_disk(parttypes: &[&str]) -> Device {
        Device {
            name: "vda".into(),
            serial: None,
            model: None,
            partlabel: None,
            parttype: None,
            partuuid: None,
            partn: None,
            size: 10737418240,
            maj_min: None,
            start: None,
            label: None,
            fstype: None,
            uuid: None,
            path: Some("/dev/vda".into()),
            pttype: Some("dos".into()),
            children: Some(
                parttypes
                    .iter()
                    .enumerate()
                    .map(|(i, pt)| Device {
                        name: format!("vda{}", i + 1),
                        serial: None,
                        model: None,
                        partlabel: None,
                        parttype: Some(pt.to_string()),
                        partuuid: None,
                        partn: Some(i as u32 + 1),
                        size: 1048576,
                        maj_min: None,
                        start: Some(2048),
                        label: None,
                        fstype: None,
                        uuid: None,
                        path: None,
                        pttype: Some("dos".into()),
                        children: None,
                    })
                    .collect(),
            ),
        }
    }

    #[test]
    fn test_mbr_esp_detection() {
        // 0x06 (FAT16) is recognized as ESP
        let dev = make_mbr_disk(&["0x06"]);
        assert_eq!(dev.find_partition_of_esp().unwrap().partn, Some(1));

        // 0xef (EFI System Partition) is recognized as ESP
        let dev = make_mbr_disk(&["0x83", "0xef"]);
        assert_eq!(dev.find_partition_of_esp().unwrap().partn, Some(2));

        // No ESP types present: 0x83 (Linux) and 0x82 (swap)
        let dev = make_mbr_disk(&["0x83", "0x82"]);
        assert!(dev.find_partition_of_esp().is_err());
    }

    #[test]
    fn test_parse_sfdisk() -> Result<()> {
        let fixture = indoc::indoc! { r#"
        {
            "partitiontable": {
               "label": "gpt",
               "id": "A67AA901-2C72-4818-B098-7F1CAC127279",
               "device": "/dev/loop0",
               "unit": "sectors",
               "firstlba": 34,
               "lastlba": 20971486,
               "sectorsize": 512,
               "partitions": [
                  {
                     "node": "/dev/loop0p1",
                     "start": 2048,
                     "size": 8192,
                     "type": "9E1A2D38-C612-4316-AA26-8B49521E5A8B",
                     "uuid": "58A4C5F0-BD12-424C-B563-195AC65A25DD",
                     "name": "PowerPC-PReP-boot"
                  },{
                     "node": "/dev/loop0p2",
                     "start": 10240,
                     "size": 20961247,
                     "type": "0FC63DAF-8483-4772-8E79-3D69D8477DE4",
                     "uuid": "F51ABB0D-DA16-4A21-83CB-37F4C805AAA0",
                     "name": "root"
                  }
               ]
            }
         }
        "# };
        let table: SfDiskOutput = serde_json::from_str(&fixture).unwrap();
        assert_eq!(
            table.partitiontable.find("/dev/loop0p2").unwrap().size,
            20961247
        );
        Ok(())
    }

    #[test]
    fn test_parse_partition_number_from_suffix() {
        // Short alias like /dev/mapper/mpatha → /dev/mapper/mpatha2
        assert_eq!(
            parse_partition_number_from_suffix("/dev/mapper/mpatha", "/dev/mapper/mpatha2"),
            Some("2".into())
        );
        // With a "p" separator: /dev/mapper/mpatha → /dev/mapper/mpathap2
        assert_eq!(
            parse_partition_number_from_suffix("/dev/mapper/mpatha", "/dev/mapper/mpathap2"),
            Some("2".into())
        );
        // WWID-style name with "part" separator
        assert_eq!(
            parse_partition_number_from_suffix(
                "/dev/mapper/3600508b4001",
                "/dev/mapper/3600508b4001-part1"
            ),
            Some("1".into())
        );
        // Multi-digit partition number
        assert_eq!(
            parse_partition_number_from_suffix("/dev/mapper/mpatha", "/dev/mapper/mpatha12"),
            Some("12".into())
        );
        // ESP path doesn't share the parent prefix → None
        assert_eq!(
            parse_partition_number_from_suffix("/dev/mapper/mpatha", "/dev/sda1"),
            None
        );
        // No digits in suffix → None
        assert_eq!(
            parse_partition_number_from_suffix("/dev/mapper/mpatha", "/dev/mapper/mpathap"),
            None
        );
        // Identical paths (no suffix at all) → None
        assert_eq!(
            parse_partition_number_from_suffix("/dev/mapper/mpatha", "/dev/mapper/mpatha"),
            None
        );
    }
}
