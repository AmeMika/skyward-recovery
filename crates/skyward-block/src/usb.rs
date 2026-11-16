//! USB drives plugged into the phone's port: found among the kernel's
//! disks by the USB bus they hang from, and shown read only for the files
//! on them to be read.

use crate::mount;
use anyhow::{bail, Context, Result};
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path};

/// Where the kernel lists its disks, each a link to the device it is.
const SYS_BLOCK: &str = "/sys/block";

/// Where ueventd makes the nodes of disks and partitions.
const DEV_BLOCK: &str = "/dev/block";

/// The filesystems a USB drive may hold, in the order they are tried, with
/// what each is mounted with to give names as the drive keeps them: FAT
/// and exFAT keep long names in UTF-16, which the kernel otherwise turns
/// into a code page without Chinese.
const FILESYSTEMS: [(&str, &str); 5] =
    [("vfat", "utf8"), ("exfat", "iocharset=utf8"), ("ntfs3", "iocharset=utf8"), ("ext4", ""), ("f2fs", "")];

/// A USB drive as the kernel lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive {
    /// The disk's name, such as `sdg`.
    pub disk: String,
    /// Its partitions by name, each with its size in bytes.
    pub parts: Vec<(String, u64)>,
}

impl Drive {
    /// Whether ueventd has made the nodes of the disk and its partitions,
    /// which come a moment after the kernel lists them.
    pub fn ready(&self) -> bool {
        std::iter::once(&self.disk).chain(self.parts.iter().map(|(name, _)| name)).all(|name| Path::new(DEV_BLOCK).join(name).exists())
    }

    /// What to show of the drive, in the order it is tried: its partitions,
    /// the largest first, or the whole disk for a drive formatted without
    /// a partition table.
    fn candidates(&self) -> Vec<&str> {
        if self.parts.is_empty() {
            return vec![&self.disk];
        }
        let mut parts: Vec<&(String, u64)> = self.parts.iter().collect();
        parts.sort_by_key(|&(_, size)| std::cmp::Reverse(*size));
        parts.into_iter().map(|(name, _)| name.as_str()).collect()
    }
}

/// The USB drives plugged in that hold anything, by the names of their
/// disks. A drive pulled out while it is looked at is left out.
pub fn drives() -> Result<Vec<Drive>> {
    drives_in(Path::new(SYS_BLOCK))
}

fn drives_in(listed: &Path) -> Result<Vec<Drive>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(listed).with_context(|| format!("read {}", listed.display()))? {
        let link = entry.with_context(|| format!("read {}", listed.display()))?.path();
        if let Some(drive) = drive_at(&link, true)? {
            found.push(drive);
        }
    }
    found.sort_by(|a, b| a.disk.cmp(&b.disk));
    Ok(found)
}

/// The kernel disk named `name`, whether it is on USB or an MMC slot. None
/// while it is absent or holds no media.
pub fn drive(name: &str) -> Result<Option<Drive>> {
    anyhow::ensure!(!name.is_empty() && Path::new(name).file_name().is_some_and(|one| one == name), "invalid disk name {name:?}");
    drive_at(&Path::new(SYS_BLOCK).join(name), false)
}

fn drive_at(link: &Path, usb_only: bool) -> Result<Option<Drive>> {
    let Some(device) = present(fs::canonicalize(link)).with_context(|| format!("follow {}", link.display()))? else { return Ok(None) };
    if usb_only && !on_usb(&device) {
        return Ok(None);
    }
    // A card reader with no card in it is a disk of no size.
    let Some(size) = sectors(&device)? else { return Ok(None) };
    if size == 0 {
        return Ok(None);
    }
    let Some(inside) = present(fs::read_dir(&device)).with_context(|| format!("read {}", device.display()))? else { return Ok(None) };
    let mut parts = Vec::new();
    for entry in inside {
        let dir = entry.with_context(|| format!("read {}", device.display()))?.path();
        if !dir.join("partition").is_file() {
            continue;
        }
        let Some(size) = sectors(&dir)? else { continue };
        parts.push((name(&dir)?, size * 512));
    }
    parts.sort();
    Ok(Some(Drive { disk: name(link)?, parts }))
}

/// Whether a disk's device hangs from a USB bus, `usb1` and the like, as a
/// drive plugged into the port does; the phone's own storage hangs from
/// its UFS controller.
fn on_usb(device: &Path) -> bool {
    device.components().any(|part| match part {
        Component::Normal(name) => name
            .to_str()
            .and_then(|n| n.strip_prefix("usb"))
            .is_some_and(|bus| !bus.is_empty() && bus.bytes().all(|b| b.is_ascii_digit())),
        _ => false,
    })
}

/// The size the kernel gives a disk or partition at `dir`, in 512-byte
/// sectors; none once it is gone.
fn sectors(dir: &Path) -> Result<Option<u64>> {
    let path = dir.join("size");
    let Some(text) = present(fs::read_to_string(&path)).with_context(|| format!("read {}", path.display()))? else { return Ok(None) };
    let sectors = text.trim().parse().with_context(|| format!("{} holds {text:?}", path.display()))?;
    Ok(Some(sectors))
}

fn name(path: &Path) -> Result<String> {
    Ok(path.file_name().with_context(|| format!("{} has no name", path.display()))?.to_string_lossy().into_owned())
}

/// What `r` holds, or nothing for what was gone when it was looked at.
fn present<T>(r: std::io::Result<T>) -> std::io::Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Show `drive` read only at `at`: the first of its partitions, or its
/// disk, that holds a filesystem the kernel reads. Gives the partition and
/// the filesystem shown.
pub fn mount(drive: &Drive, at: &Path) -> Result<(String, &'static str)> {
    mount_as(drive, at, true)
}

/// Show a removable card read-write at `at`.
pub fn mount_rw(drive: &Drive, at: &Path) -> Result<(String, &'static str)> {
    mount_as(drive, at, false)
}

fn mount_as(drive: &Drive, at: &Path, read_only: bool) -> Result<(String, &'static str)> {
    // A service restart leaves mounts in place. Reuse this drive's mount,
    // verifying the device number before treating it as the inserted card.
    if let Some(visible) = mount::visible(at)? {
        for name in drive.candidates() {
            if crate::devnum(Path::new(DEV_BLOCK).join(name))? == visible.device {
                let filesystem = FILESYSTEMS.iter().find(|(fs, _)| *fs == visible.filesystem).map(|&(fs, _)| fs);
                if let Some(filesystem) = filesystem {
                    mount::remount_writable(at, !read_only)?;
                    return Ok((name.to_string(), filesystem));
                }
            }
        }
        bail!("{} already holds another mounted device", at.display());
    }
    fs::create_dir_all(at).with_context(|| format!("create {}", at.display()))?;
    let mut tried = Vec::new();
    for name in drive.candidates() {
        let device = Path::new(DEV_BLOCK).join(name);
        for (filesystem, own) in FILESYSTEMS {
            let mut options: Vec<String> = vec![if read_only { "ro" } else { "rw" }, "nosuid", "nodev", "noexec"].into_iter().map(str::to_string).collect();
            if !own.is_empty() {
                options.push(own.to_string());
            }
            match mount::mount(&device, at, filesystem, &options) {
                Ok(()) => return Ok((name.to_string(), filesystem)),
                Err(e) => tried.push(format!("{name} as {filesystem}: {}", e.root_cause())),
            }
        }
    }
    bail!("{} holds no filesystem the kernel reads: {}", drive.disk, tried.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A disk at `path` under `root`'s devices, with `sectors` and these
    /// partitions, each with its sectors, listed in `root`'s block folder
    /// as the kernel lists its disks.
    fn disk(root: &Path, path: &str, sectors: u64, parts: &[(&str, u64)]) {
        let dir = root.join("devices").join(path);
        fs::create_dir_all(dir.join("queue")).unwrap();
        fs::write(dir.join("size"), format!("{sectors}\n")).unwrap();
        for (k, (part, sectors)) in parts.iter().enumerate() {
            fs::create_dir_all(dir.join(part)).unwrap();
            fs::write(dir.join(part).join("partition"), format!("{}\n", k + 1)).unwrap();
            fs::write(dir.join(part).join("size"), format!("{sectors}\n")).unwrap();
        }
        let name = dir.file_name().unwrap().to_owned();
        fs::create_dir_all(root.join("block")).unwrap();
        symlink(&dir, root.join("block").join(name)).unwrap();
    }

    #[test]
    fn only_disks_on_a_usb_bus_that_hold_something_are_drives() {
        let root = std::env::temp_dir().join(format!("skyward-usb-{}", std::process::id()));
        disk(&root, "platform/soc/1d84000.ufshc/host0/target0:0:0/0:0:0:0/block/sda", 61_440_000, &[("sda1", 8), ("sda2", 2_048)]);
        disk(&root, "virtual/block/loop0", 2_048, &[]);
        disk(&root, "platform/soc/7824900.sdhci/mmc_host/mmc1/mmc1:0001/block/mmcblk1", 60_000_000, &[("mmcblk1p1", 59_000_000)]);
        let usb = "platform/soc/a600000.ssusb/a600000.dwc3/xhci-hcd.0.auto/usb1";
        disk(&root, &format!("{usb}/1-1/1-1:1.0/host1/target1:0:0/1:0:0:0/block/sdg"), 60_437_492, &[("sdg2", 60_000_000), ("sdg1", 409_600)]);
        disk(&root, &format!("{usb}/1-2/1-2:1.0/host2/target2:0:0/2:0:0:0/block/sdh"), 0, &[]);
        let found = drives_in(&root.join("block")).unwrap();
        let sdg = Drive { disk: "sdg".into(), parts: vec![("sdg1".into(), 409_600 * 512), ("sdg2".into(), 60_000_000 * 512)] };
        assert_eq!(found, [sdg], "the phone's storage, a loop device and an empty card reader are left out");
        let card = drive_at(&root.join("block/mmcblk1"), false).unwrap().unwrap();
        assert_eq!(card.disk, "mmcblk1");
        assert_eq!(card.parts, [("mmcblk1p1".into(), 59_000_000 * 512)]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_largest_partition_is_tried_first_and_a_disk_without_any_whole() {
        let mac = Drive { disk: "sdg".into(), parts: vec![("sdg1".into(), 209_715_200), ("sdg2".into(), 63_000_000_000)] };
        assert_eq!(mac.candidates(), ["sdg2", "sdg1"], "the data before the EFI partition in front of it");
        let floppy = Drive { disk: "sdh".into(), parts: Vec::new() };
        assert_eq!(floppy.candidates(), ["sdh"]);
    }
}
