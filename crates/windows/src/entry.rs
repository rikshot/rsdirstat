//! What a scan counts of an entry, from what a folder listing, or the MFT record behind it, says of
//! it: the rules both ways of scanning share.

use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_DESCRIPTOR,
    FILE_ID_DESCRIPTOR_0, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileAttributeTagInfo, FileIdType, GetFileInformationByHandleEx, OpenFileById,
};

// Reparse tags for cloud placeholders created by the Cloud Files API (OneDrive, and other providers
// built on it). They share the base 0x9000001A; a provider index occupies the CLOUD_MASK (0xF000)
// nibble, spanning 0x9000001A..=0x9000F01A. Values mirror windows-sys' IO_REPARSE_TAG_CLOUD /
// IO_REPARSE_TAG_CLOUD_MASK (in Win32_System_SystemServices), defined here so we needn't pull that
// whole bindings module in for two integers.
const IO_REPARSE_TAG_CLOUD: u32 = 0x9000_001A;
const IO_REPARSE_TAG_CLOUD_MASK: u32 = 0x0000_F000;

/// The reparse tag bit marking a name surrogate: an entry that stands for another file or directory
/// (a symlink, a junction, a volume mount point), as the SDK's `IsReparseTagNameSurrogate` tests.
const REPARSE_TAG_NAME_SURROGATE: u32 = 0x2000_0000;

/// The reparse tag bit Microsoft's own tags have, as the SDK's `IsReparseTagMicrosoft` tests.
const REPARSE_TAG_MICROSOFT: u32 = 0x8000_0000;

/// A Windows FILETIME (100 ns ticks since 1601-01-01) in Unix seconds.
pub fn filetime_to_unix(ft: i64) -> i64 {
    const EPOCH_DIFF: i64 = 11_644_473_600;
    (ft / 10_000_000) - EPOCH_DIFF
}

/// What to do with one enumerated directory entry.
#[derive(Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A directory to descend into (a plain directory, or a cloud-placeholder directory).
    Directory,
    /// A file contributing `size` bytes — 0 for an online-only (dehydrated) cloud placeholder.
    File { size: u64 },
    /// A symlink / junction / mount point (a name surrogate): neither counted nor traversed, to
    /// avoid double-counting and traversal loops.
    Skip,
}

/// A cloud-placeholder reparse tag shares the base `IO_REPARSE_TAG_CLOUD` with a provider index in
/// the `IO_REPARSE_TAG_CLOUD_MASK` nibble; mask it out and compare to the base.
fn is_cloud_reparse_tag(tag: u32) -> bool {
    tag & !IO_REPARSE_TAG_CLOUD_MASK == IO_REPARSE_TAG_CLOUD
}

/// What a scan goes by to count an entry, from a folder listing (`Entry::listed`) or its MFT record.
pub struct Entry {
    pub is_dir: bool,
    /// Its reparse tag, if it's a reparse point.
    pub reparse_tag: Option<u32>,
    /// Whether its data isn't all on this disk (`FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`).
    pub recall_on_access: bool,
    /// Its logical size (`EndOfFile`), and how much of the disk is allocated for it.
    pub size: u64,
    pub allocated: u64,
}

impl Entry {
    /// An entry as a folder listing gives it: its attributes, the reparse tag the listing overlays
    /// onto its `EaSize` field (meaningful only for a reparse point, per [MS-FSCC]), and its sizes.
    pub fn listed(attributes: u32, reparse_tag: u32, end_of_file: i64, allocation_size: i64) -> Self {
        Self {
            is_dir: attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
            reparse_tag: (attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0).then_some(reparse_tag),
            recall_on_access: attributes & FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS != 0,
            size: end_of_file as u64,
            allocated: allocation_size as u64,
        }
    }

    /// What to do with the entry.
    ///
    /// Only reparse points that stand for another name (symlinks, junctions, mount points) are
    /// skipped. The others are the file or directory itself, with its data kept differently, and are
    /// counted like any other: cloud placeholders (OneDrive et al., files *and* folders),
    /// deduplicated files, files compressed by Windows (WOF, as Compact OS uses), and files tiered by
    /// Azure File Sync.
    pub fn kind(&self) -> EntryKind {
        // Name surrogates redirect elsewhere — skip them to avoid double-counting and traversal loops.
        if self
            .reparse_tag
            .is_some_and(|tag| tag & REPARSE_TAG_NAME_SURROGATE != 0)
        {
            return EntryKind::Skip;
        }

        if self.is_dir {
            return EntryKind::Directory;
        }

        // A file whose data isn't all on this disk counts 0, though EndOfFile still reports its
        // full "online" length: an online-only cloud placeholder, or a file tiered off by Azure
        // File Sync. FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS says so, like macOS's SF_DATALESS; a
        // partly downloaded file has it too. It's what folder listings leave to go by: to a program
        // that hasn't asked to see placeholders, Windows lists them as plain files, without their
        // reparse point (Dropbox's, on the test box). A cloud placeholder listed with nothing
        // allocated counts 0 as well. Any other file, including a materialized placeholder or a
        // sparse, deduplicated or compressed file, counts at its logical size, matching the rest of
        // the tree.
        let not_here =
            self.recall_on_access || (self.reparse_tag.is_some_and(is_cloud_reparse_tag) && self.allocated == 0);
        EntryKind::File {
            size: if not_here { 0 } else { self.size },
        }
    }
}

/// Whether what a folder listing gives as a reparse point's tag may instead be the size of its
/// extended attributes. NTFS's folder index has room for one or the other, and a reparse point with
/// extended attributes (as Linux's NTFS driver gives the symlinks it makes) keeps their size there.
/// A size looks like a tag without the Microsoft bit: one of those, rare as it is, is asked about too.
pub fn may_be_ea_size(listed_tag: u32) -> bool {
    listed_tag & REPARSE_TAG_MICROSOFT == 0
}

/// The reparse tag of the file with id `file_id` on `volume`'s volume, asked of the file itself:
/// opened by id, since Windows can't open every name (Linux allows `:` in them), and as itself rather
/// than what it points to, only to read its attributes, so a cloud file isn't downloaded.
pub fn reparse_tag_by_id(volume: &OwnedHandle, file_id: i64) -> Option<u32> {
    let id = FILE_ID_DESCRIPTOR {
        dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: FileIdType,
        Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: file_id },
    };
    let handle = unsafe {
        OpenFileById(
            volume.as_raw_handle(),
            &id,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut info: FILE_ATTRIBUTE_TAG_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileAttributeTagInfo,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    (ok != 0).then_some(info.ReparseTag)
}

#[cfg(test)]
mod tests {
    use super::*;

    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_UNPINNED};

    const DIR: u32 = FILE_ATTRIBUTE_DIRECTORY;
    const REPARSE: u32 = FILE_ATTRIBUTE_REPARSE_POINT;
    // A cloud tag with a non-zero provider index in the CLOUD_MASK nibble, to prove masking works.
    const CLOUD_TAG_3: u32 = 0x9000_301A;
    const SYMLINK_TAG: u32 = 0xA000_000C; // IO_REPARSE_TAG_SYMLINK
    const MOUNT_POINT_TAG: u32 = 0xA000_0003; // IO_REPARSE_TAG_MOUNT_POINT (junction)
    const LX_SYMLINK_TAG: u32 = 0xA000_001D; // IO_REPARSE_TAG_LX_SYMLINK (WSL)
    const DEDUP_TAG: u32 = 0x8000_0013; // IO_REPARSE_TAG_DEDUP
    const WOF_TAG: u32 = 0x8000_0017; // IO_REPARSE_TAG_WOF
    const STORAGE_SYNC_TAG: u32 = 0x8000_001E; // IO_REPARSE_TAG_STORAGE_SYNC (Azure File Sync)

    /// What the walk does with an entry listed with these attributes, tag and sizes.
    fn classify(attributes: u32, reparse_tag: u32, end_of_file: i64, allocation_size: i64) -> EntryKind {
        Entry::listed(attributes, reparse_tag, end_of_file, allocation_size).kind()
    }

    #[test]
    fn regular_file_counts_logical_size() {
        // attrs 0, some data on disk (allocation 4096).
        assert_eq!(classify(0, 0, 4096, 4096), EntryKind::File { size: 4096 });
    }

    #[test]
    fn regular_directory_recurses() {
        assert_eq!(classify(DIR, 0, 0, 0), EntryKind::Directory);
    }

    #[test]
    fn sparse_regular_file_is_not_zeroed() {
        // A non-cloud sparse file has 0 bytes allocated but a large logical length; under the tool's
        // apparent-size semantics it must still count at EndOfFile, not be mistaken for a placeholder.
        assert_eq!(classify(0, 0, 1_000_000, 0), EntryKind::File { size: 1_000_000 });
    }

    #[test]
    fn online_only_cloud_file_counts_zero() {
        // Dehydrated placeholder: cloud reparse tag, full online length in EndOfFile, 0 on disk.
        let entry = classify(REPARSE, IO_REPARSE_TAG_CLOUD, 10_000_000, 0);
        assert_eq!(entry, EntryKind::File { size: 0 });
    }

    #[test]
    fn a_file_whose_data_is_not_here_counts_zero() {
        // Dropbox's online-only file as a folder listing shows it: a plain file (its reparse point
        // hidden), unpinned, with the Cloud Files API's recall flag.
        let attrs = FILE_ATTRIBUTE_ARCHIVE | FILE_ATTRIBUTE_UNPINNED | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
        assert_eq!(classify(attrs, 0xE0, 6_653_604, 0), EntryKind::File { size: 0 });
        // Partly downloaded, or tiered off by Azure File Sync: not all here either.
        let attrs = REPARSE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
        assert_eq!(
            classify(attrs, CLOUD_TAG_3, 68_515_244, 4096),
            EntryKind::File { size: 0 }
        );
        assert_eq!(
            classify(attrs, STORAGE_SYNC_TAG, 1_000_000, 0),
            EntryKind::File { size: 0 }
        );
    }

    #[test]
    fn hydrated_cloud_file_counts_logical_size() {
        // Locally-available placeholder: cloud reparse tag, data on disk (allocation > 0).
        assert_eq!(classify(REPARSE, CLOUD_TAG_3, 173, 4096), EntryKind::File { size: 173 });
    }

    #[test]
    fn cloud_directory_recurses() {
        // Folders are placeholders too; we must descend rather than skip the whole subtree.
        assert_eq!(classify(DIR | REPARSE, CLOUD_TAG_3, 0, 0), EntryKind::Directory);
    }

    #[test]
    fn symlink_and_junction_are_skipped() {
        assert_eq!(classify(REPARSE, SYMLINK_TAG, 0, 0), EntryKind::Skip);
        assert_eq!(classify(DIR | REPARSE, MOUNT_POINT_TAG, 0, 0), EntryKind::Skip);
        assert_eq!(classify(REPARSE, LX_SYMLINK_TAG, 0, 0), EntryKind::Skip);
    }

    #[test]
    fn reparse_points_holding_their_own_data_count_at_logical_size() {
        // Their data lives elsewhere (a chunk store, a compressed stream, the cloud), so nothing is
        // allocated to the file itself; they are still files, not redirections.
        for tag in [DEDUP_TAG, WOF_TAG, STORAGE_SYNC_TAG] {
            assert_eq!(
                classify(REPARSE, tag, 1_000_000, 0),
                EntryKind::File { size: 1_000_000 },
                "{tag:#x}"
            );
        }
    }

    #[test]
    fn a_listed_tag_without_the_microsoft_bit_may_be_an_ea_size() {
        // A Linux-made symlink's listing: 0x2D bytes of extended attributes where its tag would be.
        assert!(may_be_ea_size(0x2D));
        for tag in [SYMLINK_TAG, MOUNT_POINT_TAG, LX_SYMLINK_TAG, CLOUD_TAG_3, WOF_TAG] {
            assert!(!may_be_ea_size(tag), "{tag:#x}");
        }
    }

    #[test]
    fn cloud_tag_detection_masks_provider_nibble() {
        assert!(is_cloud_reparse_tag(IO_REPARSE_TAG_CLOUD));
        assert!(is_cloud_reparse_tag(0x9000_301A));
        assert!(is_cloud_reparse_tag(0x9000_F01A));
        assert!(!is_cloud_reparse_tag(SYMLINK_TAG));
        assert!(!is_cloud_reparse_tag(MOUNT_POINT_TAG));
        assert!(!is_cloud_reparse_tag(0));
    }
}
