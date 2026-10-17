//! The mount table, from /proc/self/mountinfo: read by name, so asking costs no call into any mounted
//! filesystem, which on a network share whose server is gone would block.

/// What /proc/self/mountinfo says of one mount, as in
/// `36 35 98:0 /mnt1 /mnt/parent rw,noatime master:1 - ext3 /dev/root rw,errors=continue`.
pub struct Mount<'a> {
    pub id: u64,
    /// The filesystem's device, as (major, minor).
    pub device: (u32, u32),
    /// The folder of the filesystem that is mounted: `/` for all of it, a subfolder for a bind mount
    /// of part of it (or for a btrfs subvolume). Escaped as `mount_point` is.
    pub root: &'a str,
    /// Where it is mounted, with a space, tab, newline or backslash escaped as octal (`\040`).
    pub mount_point: &'a str,
    pub fs_type: &'a str,
    /// What it is mounted from: a device (`/dev/nvme0n1p5`), a share (`//nas/share`), a ZFS dataset,
    /// or whatever a filesystem without one calls itself (`proc`).
    pub source: &'a str,
}

impl<'a> Mount<'a> {
    /// The mount a line of /proc/self/mountinfo describes, or `None` if it doesn't read as one.
    pub fn parse(line: &'a str) -> Option<Self> {
        // The id, the parent's, the device, the root, the mount point, its options, optional fields
        // (`shared:13`), then `-`, the filesystem type, the source and the filesystem's options.
        let fields: Vec<&str> = line.split(' ').collect();
        let dash = fields.iter().position(|&field| field == "-")?;
        let (major, minor) = fields.get(2)?.split_once(':')?;
        Some(Self {
            id: fields.first()?.parse().ok()?,
            device: (major.parse().ok()?, minor.parse().ok()?),
            root: fields.get(3)?,
            mount_point: fields.get(4)?,
            fs_type: fields.get(dash + 1)?,
            source: fields.get(dash + 2)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_with_and_without_optional_fields_parse() {
        let mount =
            Mount::parse("36 35 98:0 /mnt1 /mnt/parent rw,noatime master:1 shared:2 - ext3 /dev/root rw").unwrap();
        assert_eq!((mount.id, mount.device), (36, (98, 0)));
        assert_eq!((mount.root, mount.mount_point), ("/mnt1", "/mnt/parent"));
        assert_eq!((mount.fs_type, mount.source), ("ext3", "/dev/root"));
        let mount = Mount::parse("22 1 0:21 / /proc rw,nosuid - proc proc rw").unwrap();
        assert_eq!((mount.fs_type, mount.source, mount.root), ("proc", "proc", "/"));
        assert!(Mount::parse("not a mount").is_none());
    }
}
