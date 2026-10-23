//! The USB drives that are connected, and whether each holds an Engine library at its
//! root. Engine's players read the library from a folder called `Engine Library` at
//! the top of the drive, so that is the only place looked at.
//!
//! Only Linux is supported: a drive is a mounted filesystem whose block device hangs
//! off a USB port. Elsewhere the list is always empty. A drive can be unmounted (through
//! the desktop's UDisks service, so no root is needed), which writes out what is still in
//! memory first.

use std::path::PathBuf;

/// The folder at the root of a drive that holds an Engine library.
const LIBRARY_DIR: &str = "Engine Library";
/// What makes the folder a library and not just a folder with that name.
const MAIN_DATABASE: &str = "Database2/m.db";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive {
    /// What the drive is called: the name of its mount point, which is the volume label
    /// for drives mounted by the desktop.
    pub name: String,
    pub mount: PathBuf,
    /// The `Engine Library` folder on the drive, if it has one.
    pub library: Option<PathBuf>,
    /// The block device the drive's filesystem is on (`/dev/sdc1`), which is what is
    /// unmounted.
    pub device: Option<PathBuf>,
}

/// Unmounts the filesystem on `device` (`Drive::device`) once everything written to it
/// has reached the drive, so that it can be pulled out. Fails with the reason if
/// something is still using it.
pub async fn unmount(device: PathBuf) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        linux::unmount(&device).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = device;
        Err("unmounting is only supported on Linux".into())
    }
}

/// The connected USB drives, sorted by name. This reads the disk, so it is not for the
/// UI thread: a drive that is struggling can make it wait.
pub fn detect() -> Vec<Drive> {
    #[cfg(target_os = "linux")]
    {
        linux::detect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::path::{Component, Path, PathBuf};

    use super::{Drive, LIBRARY_DIR, MAIN_DATABASE};

    /// A mounted filesystem backed by a block device.
    #[derive(Debug, PartialEq, Eq)]
    struct Mount {
        /// `major:minor` of the device, which is also its name under `/sys/dev/block`.
        device: String,
        point: PathBuf,
        /// The block device file (`/dev/sdc1`), when the kernel names one.
        source: Option<PathBuf>,
    }

    pub fn detect() -> Vec<Drive> {
        let Ok(mountinfo) = fs::read_to_string("/proc/self/mountinfo") else {
            return Vec::new();
        };
        let mut drives: Vec<Drive> = parse_mountinfo(&mountinfo)
            .into_iter()
            .filter(|mount| is_usb(Path::new("/sys/dev/block"), &mount.device))
            .map(|mount| drive(mount.point, mount.source))
            .collect();
        drives.sort_by_key(|drive| drive.name.to_lowercase());
        drives
    }

    fn drive(mount: PathBuf, device: Option<PathBuf>) -> Drive {
        let library = mount.join(LIBRARY_DIR);
        let has_library = library.join(MAIN_DATABASE).is_file();
        Drive {
            name: mount.file_name().map_or_else(
                || mount.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            ),
            library: has_library.then_some(library),
            mount,
            device,
        }
    }

    /// Where UDisks has the block device `/dev/<name>`. Only plain names (`sdc1`) are
    /// handled: UDisks escapes the others, and a USB drive has plain ones.
    fn block_object_path(device: &Path) -> Option<String> {
        let name = device.strip_prefix("/dev").ok()?.to_str()?;
        (!name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric()))
            .then(|| format!("/org/freedesktop/UDisks2/block_devices/{name}"))
    }

    pub async fn unmount(device: &Path) -> Result<(), String> {
        use std::collections::HashMap;

        let path = block_object_path(device)
            .ok_or_else(|| format!("{} cannot be unmounted from here", device.display()))?;
        let connection = zbus::Connection::system()
            .await
            .map_err(|error| format!("the system bus is not available: {error}"))?;
        let options: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        connection
            .call_method(
                Some("org.freedesktop.UDisks2"),
                path.as_str(),
                Some("org.freedesktop.UDisks2.Filesystem"),
                "Unmount",
                &(options,),
            )
            .await
            .map(|_| ())
            .map_err(|error| match error {
                zbus::Error::MethodError(_, Some(message), _) => message,
                other => other.to_string(),
            })
    }

    /// The mounts of whole filesystems of real devices. A device that is mounted in more
    /// than one place counts once, and so do bind mounts of a folder (`root` is not `/`).
    fn parse_mountinfo(text: &str) -> Vec<Mount> {
        let mut mounts: Vec<Mount> = Vec::new();
        for line in text.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // id parent major:minor root mount-point options [optional...] - fstype source ...
            let [_, _, device, root, point, ..] = fields[..] else {
                continue;
            };
            // Filesystems without a device (tmpfs, fuse, btrfs subvolumes...) have major 0.
            if root != "/" || device.starts_with("0:") {
                continue;
            }
            if mounts.iter().any(|mount| mount.device == device) {
                continue;
            }
            // After the optional fields, a `-`, the file system type and the source.
            let source = fields
                .iter()
                .position(|field| *field == "-")
                .and_then(|dash| fields.get(dash + 2))
                .filter(|source| source.starts_with('/'))
                .map(|source| PathBuf::from(unescape(source)));
            mounts.push(Mount {
                device: device.to_string(),
                point: PathBuf::from(unescape(point)),
                source,
            });
        }
        mounts
    }

    /// The kernel writes a space, tab, newline or backslash in a path as a backslash and
    /// the three octal digits of the byte.
    fn unescape(field: &str) -> String {
        let bytes = field.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            let octal = bytes
                .get(index + 1..index + 4)
                .filter(|_| bytes[index] == b'\\')
                .and_then(|digits| std::str::from_utf8(digits).ok())
                .and_then(|digits| u8::from_str_radix(digits, 8).ok());
            match octal {
                Some(byte) => {
                    out.push(byte);
                    index += 4;
                }
                None => {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Whether the block device is on a USB port: its place in the device tree (what
    /// `<sys_block>/<device>` links to) passes through a `usbN` node.
    fn is_usb(sys_block: &Path, device: &str) -> bool {
        let Ok(path) = fs::canonicalize(sys_block.join(device)) else {
            return false;
        };
        path.components().any(|component| match component {
            Component::Normal(name) => name.to_str().is_some_and(|name| {
                name.strip_prefix("usb").is_some_and(|rest| {
                    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
                })
            }),
            _ => false,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const MOUNTINFO: &str = "\
24 1 254:2 / / rw,relatime - ext4 /dev/mapper/root rw
25 24 254:2 /nix /nix rw,relatime - ext4 /dev/mapper/root rw
26 24 0:44 / /mnt/storage rw,relatime - btrfs /dev/mapper/storage rw
27 24 8:1 / /run/media/me/DJ\\040MAIN rw,nosuid master:609 - vfat /dev/sda1 rw
28 24 8:1 / /mnt/second-mount rw - vfat /dev/sda1 rw
29 24 8:17 / /run/media/me/STICK rw - exfat /dev/sdb1 rw
30 24 0:26 / /run rw - tmpfs tmpfs rw
";

        #[test]
        fn reads_real_devices_once_and_unescapes_names() {
            let mounts = parse_mountinfo(MOUNTINFO);
            let seen: Vec<(&str, &str)> = mounts
                .iter()
                .map(|mount| (mount.device.as_str(), mount.point.to_str().unwrap()))
                .collect();
            assert_eq!(
                seen,
                [
                    ("254:2", "/"),
                    ("8:1", "/run/media/me/DJ MAIN"),
                    ("8:17", "/run/media/me/STICK"),
                ]
            );
        }

        #[test]
        fn knows_the_block_device_of_a_mount() {
            let mounts = parse_mountinfo(MOUNTINFO);
            let sources: Vec<Option<&str>> = mounts
                .iter()
                .map(|mount| mount.source.as_deref().and_then(Path::to_str))
                .collect();
            assert_eq!(
                sources,
                [
                    Some("/dev/mapper/root"),
                    Some("/dev/sda1"),
                    Some("/dev/sdb1")
                ]
            );
        }

        #[test]
        fn udisks_names_a_plain_block_device_by_its_name() {
            assert_eq!(
                block_object_path(Path::new("/dev/sdc1")).as_deref(),
                Some("/org/freedesktop/UDisks2/block_devices/sdc1")
            );
            // Anything else would need UDisks' escaping, or is not a device file.
            assert_eq!(block_object_path(Path::new("/dev/mapper/root")), None);
            assert_eq!(block_object_path(Path::new("/dev/")), None);
            assert_eq!(block_object_path(Path::new("/tmp/sdc1")), None);
        }

        #[test]
        fn unescapes_only_complete_octal_codes() {
            assert_eq!(unescape("a\\040b\\134c"), "a b\\c");
            assert_eq!(unescape("trailing\\04"), "trailing\\04");
            assert_eq!(unescape("plain"), "plain");
        }

        struct TempDir(PathBuf);

        impl TempDir {
            fn new(name: &str) -> Self {
                let path = std::env::temp_dir().join(format!(
                    "open-engine-lib-drives-{name}-{}",
                    std::process::id()
                ));
                let _ = fs::remove_dir_all(&path);
                fs::create_dir_all(&path).unwrap();
                Self(path)
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn tells_usb_devices_from_others() {
            let temp = TempDir::new("usb");
            let devices = temp.0.join("devices");
            let usb = devices.join("pci0000:00/0000:00:14.0/usb2/2-3/block/sda");
            let nvme = devices.join("pci0000:00/nvme/nvme0/nvme0n1");
            let hub = devices.join("pci0000:00/usbhub/block/sdc");
            for dir in [&usb, &nvme, &hub] {
                fs::create_dir_all(dir).unwrap();
            }
            let sys_block = temp.0.join("block");
            fs::create_dir_all(&sys_block).unwrap();
            for (name, target) in [("8:0", &usb), ("259:0", &nvme), ("8:32", &hub)] {
                std::os::unix::fs::symlink(target, sys_block.join(name)).unwrap();
            }

            assert!(is_usb(&sys_block, "8:0"));
            assert!(!is_usb(&sys_block, "259:0"));
            // `usbhub` is not a port, and a device the kernel doesn't know is not USB.
            assert!(!is_usb(&sys_block, "8:32"));
            assert!(!is_usb(&sys_block, "9:9"));
        }

        #[test]
        fn finds_the_library_at_the_root_only() {
            let temp = TempDir::new("library");

            let with = temp.0.join("WITH");
            fs::create_dir_all(with.join("Engine Library/Database2")).unwrap();
            fs::write(with.join("Engine Library/Database2/m.db"), b"db").unwrap();
            let found = drive(with.clone(), None);
            assert_eq!(found.name, "WITH");
            assert_eq!(found.library, Some(with.join("Engine Library")));

            // A folder of that name without the database isn't a library.
            let empty = temp.0.join("EMPTY");
            fs::create_dir_all(empty.join("Engine Library")).unwrap();
            assert_eq!(drive(empty, None).library, None);

            // And one further down doesn't count: players only look at the root.
            let nested = temp.0.join("NESTED");
            fs::create_dir_all(nested.join("Music/Engine Library/Database2")).unwrap();
            fs::write(nested.join("Music/Engine Library/Database2/m.db"), b"db").unwrap();
            assert_eq!(drive(nested, None).library, None);
        }
    }
}
