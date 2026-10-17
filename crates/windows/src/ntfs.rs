//! Reading an NTFS volume's master file table (MFT). Every file and folder on the volume is a record
//! in it, holding its names, parent folders, size and times, so reading it straight off the disk lists
//! the whole volume without opening a single folder. On Windows that is several times faster than the
//! folder walk, whose cost is Windows' own work per folder; it needs administrator rights to read the
//! volume. This module parses the records `mft` reads and turns them into the scan's events,
//! counting what the folder walk counts.

use std::path::Path;
use std::sync::mpsc::Sender;

use rsdirstat_core::scan::{DirFiles, ScanEvent, node_id, root_name};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_REPARSE_POINT};

use crate::entry::{Entry, EntryKind, filetime_to_unix};

/// Records below this are NTFS's own metadata files ($MFT, $LogFile, ..., and $Extend, which holds
/// more of them), which folder listings never show.
const FIRST_USER_RECORD: usize = 16;
/// Each 512 bytes of a record end in a fixup, whatever the disk's sector size.
const FIXUP_STRIDE: usize = 512;

const STANDARD_INFORMATION: u32 = 0x10;
const FILE_NAME: u32 = 0x30;
const DATA: u32 = 0x80;
const REPARSE_POINT: u32 = 0xC0;
const END_OF_ATTRIBUTES: u32 = 0xFFFF_FFFF;
/// `$FILE_NAME`'s namespace for a DOS 8.3 short name that has a long one besides.
const DOS_NAMESPACE: u8 = 2;
/// An attribute's flags for a compressed or a sparse stream, whose header then also has the space
/// the stream takes on the disk.
const COMPRESSED_OR_SPARSE: u16 = 0x0001 | 0x8000;

/// The record of a volume's root folder.
const ROOT_FOLDER: usize = 5;

/// Whether the file reference `reference` is a volume's root folder.
pub fn is_volume_root(reference: u64) -> bool {
    record_number(reference) == ROOT_FOLDER
}

/// The record number in a file reference; the top 16 bits are the record's sequence number.
fn record_number(reference: u64) -> usize {
    (reference & 0xFFFF_FFFF_FFFF) as usize
}

fn reference(number: usize, sequence: u16) -> u64 {
    (u64::from(sequence) << 48) | number as u64
}

/// What a record holds that the scan needs, or for an extension record, what it adds to its base.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileRecord {
    pub is_dir: bool,
    /// Bumped each time the record is reused for another file. References to the record (its
    /// parent folder's, in a name) carry it, so a stale one shows.
    pub sequence: u16,
    /// Whether the unnamed data stream's sizes were in this record: an extension record may carry
    /// them instead of the base.
    pub has_data: bool,
    /// The unnamed data stream's logical size (what `EndOfFile` and `st_size` report), and how much
    /// is allocated for it.
    pub size: u64,
    pub allocated: u64,
    /// The last modification, in Unix seconds.
    pub mtime: i64,
    /// Its attributes, as `$STANDARD_INFORMATION` keeps them up to date.
    pub attributes: u32,
    /// Its reparse tag, if its records hold it: see `Mft::send_scan`.
    pub reparse_tag: u32,
    /// Each name with its parent folder's reference; DOS short names with a long one are left out.
    pub names: Vec<(u64, Box<str>)>,
}

impl FileRecord {
    /// What the folder walk does with this file.
    fn kind(&self) -> EntryKind {
        Entry {
            is_dir: self.is_dir,
            // Only a reparse point has a tag.
            reparse_tag: (self.reparse_tag != 0).then_some(self.reparse_tag),
            recall_on_access: self.attributes & FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS != 0,
            size: self.size,
            allocated: self.allocated,
        }
        .kind()
    }
}

/// A parsed MFT record.
#[derive(Clone, Debug, PartialEq)]
pub enum Record {
    Base(FileRecord),
    /// An extension record: attributes that didn't fit in the base record, whose reference is `base`.
    Extension {
        base: u64,
        attributes: FileRecord,
    },
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

pub fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

pub fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// Where a record's fixup array is, and how many entries it has, if it looks like a record.
fn fixup_array(record: &[u8]) -> Option<(usize, usize)> {
    if record.len() < 0x30 || &record[..4] != b"FILE" {
        return None;
    }
    let fixups = u16_at(record, 4) as usize;
    let fixup_count = u16_at(record, 6) as usize;
    if fixup_count == 0 || fixups + 2 * fixup_count > record.len() || (fixup_count - 1) * FIXUP_STRIDE > record.len() {
        return None;
    }
    Some((fixups, fixup_count))
}

/// Whether a record as read from the disk was caught being rewritten. Each stride's last two bytes
/// were swapped for the record's update sequence number, and the originals kept in the fixup array:
/// a mismatch means that stride was written after the others. Read again, it won't be.
pub fn is_torn(record: &[u8]) -> bool {
    fixup_array(record).is_some_and(|(fixups, fixup_count)| torn(record, fixups, fixup_count))
}

/// `is_torn`, for a record whose fixup array has been found.
fn torn(record: &[u8], fixups: usize, fixup_count: usize) -> bool {
    (1..fixup_count).any(|i| record[i * FIXUP_STRIDE - 2..i * FIXUP_STRIDE] != record[fixups..fixups + 2])
}

/// Parse one MFT record as read from the disk, undoing its fixups in place. `None` for a record not
/// in use, and for one torn by a write in progress while it was read.
pub fn parse_record(record: &mut [u8]) -> Option<Record> {
    let (fixups, fixup_count) = fixup_array(record)?;
    if torn(record, fixups, fixup_count) {
        return None;
    }
    for i in 1..fixup_count {
        let end = i * FIXUP_STRIDE;
        record[end - 2] = record[fixups + 2 * i];
        record[end - 1] = record[fixups + 2 * i + 1];
    }
    let flags = u16_at(record, 0x16);
    if flags & 1 == 0 {
        return None;
    }
    let mut file = FileRecord {
        is_dir: flags & 2 != 0,
        sequence: u16_at(record, 0x10),
        ..FileRecord::default()
    };
    let base = u64_at(record, 0x20);
    let used = (u32_at(record, 0x18) as usize).min(record.len());

    let mut offset = u16_at(record, 0x14) as usize;
    while offset + 16 <= used {
        let kind = u32_at(record, offset);
        let length = u32_at(record, offset + 4) as usize;
        if kind == END_OF_ATTRIBUTES || length < 16 || offset + length > used {
            break;
        }
        let attribute = &record[offset..offset + length];
        offset += length;
        let resident = attribute[8] == 0;
        let unnamed = attribute[9] == 0;
        // A resident attribute's value: its length at 0x10, where it starts at 0x14.
        let value = || {
            if attribute.len() < 0x18 {
                return &[][..];
            }
            let start = u16_at(attribute, 0x14) as usize;
            let end = start + u32_at(attribute, 0x10) as usize;
            attribute.get(start..end).unwrap_or(&[])
        };
        match kind {
            STANDARD_INFORMATION if resident => {
                let value = value();
                if value.len() >= 0x24 {
                    file.mtime = filetime_to_unix(u64_at(value, 8) as i64);
                    file.attributes = u32_at(value, 0x20);
                }
            }
            FILE_NAME if resident => {
                let value = value();
                if value.len() < 0x42 {
                    continue;
                }
                let length = value[0x40] as usize;
                if value[0x41] == DOS_NAMESPACE || value.len() < 0x42 + 2 * length {
                    continue;
                }
                let units = value[0x42..0x42 + 2 * length]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&unit| u16::from_le_bytes(unit));
                // Room for an ASCII name, the usual kind, so it's allocated once at its size.
                let mut name = String::with_capacity(length);
                name.extend(char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)));
                // Most files have one name, and a first push would make room for four.
                file.names.reserve_exact(1);
                file.names.push((u64_at(value, 0), name.into_boxed_str()));
            }
            DATA if unnamed && resident => {
                file.has_data = true;
                file.size = value().len() as u64;
                file.allocated = file.size;
            }
            // A large stream's extents can span records; the one starting at VCN 0 has the sizes. A
            // compressed or sparse one's allocated size counts what isn't on the disk, which its header
            // then says apart: a dehydrated cloud placeholder has nothing there.
            DATA if unnamed && attribute.len() >= 0x40 && u64_at(attribute, 0x10) == 0 => {
                file.has_data = true;
                file.allocated = if u16_at(attribute, 0x0C) & COMPRESSED_OR_SPARSE != 0 && attribute.len() >= 0x48 {
                    u64_at(attribute, 0x40)
                } else {
                    u64_at(attribute, 0x28)
                };
                file.size = u64_at(attribute, 0x30);
            }
            REPARSE_POINT if resident => {
                let value = value();
                if value.len() >= 4 {
                    file.reparse_tag = u32_at(value, 0);
                }
            }
            _ => {}
        }
    }
    Some(if base == 0 {
        Record::Base(file)
    } else {
        Record::Extension { base, attributes: file }
    })
}

/// A volume's MFT records, gathered in any order as the platform reads them.
pub struct Mft {
    records: Vec<Option<FileRecord>>,
    /// Extension records: their numbers, their base records' references, and what they add.
    extensions: Vec<(usize, u64, FileRecord)>,
}

impl Mft {
    /// Room for `record_count` records, numbered from 0.
    pub fn new(record_count: usize) -> Self {
        Self {
            records: vec![None; record_count],
            extensions: Vec::new(),
        }
    }

    /// Record number `number`, as parsed. Records past the room made for them are left out: a read
    /// may run on past the end of the MFT's valid data.
    pub fn insert(&mut self, number: usize, record: Record) {
        if number >= self.records.len() {
            return;
        }
        match record {
            Record::Base(file) => self.records[number] = Some(file),
            Record::Extension { base, attributes } => self.extensions.push((number, base, attributes)),
        }
    }

    /// Send the scan of the folder `root` (a canonical path) whose file reference is `root_reference`,
    /// on the volume with serial number `volume`, between its `ScanStart` and `ScanDone`: a `Dir` per
    /// folder under it, parent first. Node ids are those the folder walk gives. Sends nothing, and
    /// returns false, if the root isn't a folder in this MFT. The names sent are taken out of it.
    ///
    /// A reparse point's tag is in its `$REPARSE_POINT`, unless that's too big to fit in its records
    /// (a symlink to a long path, say): then `tag_of` gives it, by the file's reference, or `None`
    /// if it can't. (Its names don't: they hold the tag they were made with, or in its place, the
    /// size of the file's extended attributes, as symlinks Linux makes have.)
    pub fn send_scan(
        &mut self,
        root: &Path,
        root_reference: u64,
        volume: u64,
        tx: &Sender<ScanEvent>,
        tag_of: impl FnMut(u64) -> Option<u32>,
    ) -> bool {
        self.complete_records(tag_of);
        let records = &self.records;

        // A name's parent, if that record still holds the folder the name was made in.
        let folder = |parent: u64| {
            let number = record_number(parent);
            match records.get(number) {
                Some(Some(file)) if file.is_dir && reference(number, file.sequence) == parent => Some(number),
                _ => None,
            }
        };
        let Some(root_number) = folder(root_reference) else {
            return false;
        };

        // Every folder under its parent, sorted for finding a folder's subfolders.
        let mut subfolders: Vec<(usize, usize)> = records
            .iter()
            .enumerate()
            .skip(FIRST_USER_RECORD)
            .filter_map(|(number, file)| {
                let file = file.as_ref()?;
                if file.kind() != EntryKind::Directory {
                    return None;
                }
                Some((folder(file.names.first()?.0)?, number))
            })
            .collect();
        subfolders.sort_unstable();

        // The folders under the root, parents first, each with its place in `order`.
        let mut order = vec![root_number];
        let mut place = vec![u32::MAX; records.len()];
        place[root_number] = 0;
        let mut next = 0;
        while next < order.len() {
            let parent = order[next];
            next += 1;
            let start = subfolders.partition_point(|&(p, _)| p < parent);
            for &(p, child) in &subfolders[start..] {
                if p != parent {
                    break;
                }
                // A folder appears once, however it might be reached.
                if place[child] == u32::MAX {
                    place[child] = order.len() as u32;
                    order.push(child);
                }
            }
        }
        // Each folder's reference, by its place.
        let references: Vec<u64> = order
            .iter()
            .map(|&number| reference(number, records[number].as_ref().unwrap().sequence))
            .collect();
        // A name's parent's place, if that's a folder under the root that the name was made in.
        let placed = |parent: u64| {
            let folder_place = *place.get(record_number(parent))?;
            (folder_place != u32::MAX && references[folder_place as usize] == parent).then_some(folder_place as usize)
        };

        // Each file goes in the first folder that has it under the root: a hardlink counts once.
        let mut files = vec![DirFiles::default(); order.len()];
        for file in self.records.iter_mut().skip(FIRST_USER_RECORD).flatten() {
            let EntryKind::File { size } = file.kind() else {
                continue;
            };
            let found = file
                .names
                .iter_mut()
                .find_map(|(parent, name)| Some((placed(*parent)?, name)));
            let Some((folder_place, name)) = found else { continue };
            files[folder_place].add(std::mem::take(name), size, file.mtime);
        }

        for (folder_place, (&number, DirFiles { mtime, files })) in order.iter().zip(files).enumerate() {
            let (parent, name) = if folder_place == 0 {
                (0, root_name(root))
            } else {
                // Its first name, which put it under its parent.
                let (parent, name) = &mut self.records[number].as_mut().unwrap().names[0];
                (node_id(volume, *parent), std::mem::take(name).into())
            };
            let _ = tx.send(ScanEvent::Dir {
                id: node_id(volume, references[folder_place]),
                parent,
                name,
                mtime,
                files,
            });
        }
        true
    }

    /// Fold the extension records into their base records, and fill in the reparse tags that no
    /// record holds from `tag_of` (see `send_scan`).
    fn complete_records(&mut self, mut tag_of: impl FnMut(u64) -> Option<u32>) {
        let mut extensions = std::mem::take(&mut self.extensions);
        // In record order, so a file's names come in the same order on every scan.
        extensions.sort_unstable_by_key(|&(number, ..)| number);
        for (_, base, attributes) in extensions {
            // Only into the file it was written for, not one that has reused its record since.
            let number = record_number(base);
            let Some(Some(file)) = self.records.get_mut(number) else {
                continue;
            };
            if reference(number, file.sequence) != base {
                continue;
            }
            file.names.extend(attributes.names);
            if attributes.has_data {
                file.has_data = true;
                file.size = attributes.size;
                file.allocated = attributes.allocated;
            }
            if attributes.reparse_tag != 0 {
                file.reparse_tag = attributes.reparse_tag;
            }
        }
        for (number, file) in self.records.iter_mut().enumerate().skip(FIRST_USER_RECORD) {
            if let Some(file) = file
                && file.reparse_tag == 0
                && file.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            {
                file.reparse_tag = tag_of(reference(number, file.sequence)).unwrap_or(0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc;

    use super::*;

    const RECORD_SIZE: usize = 1024;
    const VOLUME: u64 = 0xABCD;
    const SYMLINK_TAG: u32 = 0xA000_000C;
    const JUNCTION_TAG: u32 = 0xA000_0003;
    const CLOUD_TAG_3: u32 = 0x9000_301A;

    fn filetime(unix: i64) -> u64 {
        ((unix + 11_644_473_600) * 10_000_000) as u64
    }

    /// One MFT record's bytes as they are on the disk, fixups and all.
    struct Builder {
        flags: u16,
        sequence: u16,
        base: u64,
        attributes: Vec<u8>,
    }

    impl Builder {
        fn file(sequence: u16) -> Self {
            Self {
                flags: 1,
                sequence,
                base: 0,
                attributes: Vec::new(),
            }
        }

        fn folder(sequence: u16) -> Self {
            Self {
                flags: 3,
                ..Self::file(sequence)
            }
        }

        fn extension_of(base: u64) -> Self {
            Self { base, ..Self::file(1) }
        }

        fn unused() -> Self {
            Self {
                flags: 0,
                ..Self::file(1)
            }
        }

        fn resident(mut self, kind: u32, stream_name: &str, value: &[u8]) -> Self {
            let name: Vec<u8> = stream_name.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let value_offset = 0x18 + name.len();
            let length = (value_offset + value.len()).next_multiple_of(8);
            let mut attribute = vec![0u8; length];
            attribute[0..4].copy_from_slice(&kind.to_le_bytes());
            attribute[4..8].copy_from_slice(&(length as u32).to_le_bytes());
            attribute[9] = (name.len() / 2) as u8;
            attribute[0x0A..0x0C].copy_from_slice(&0x18u16.to_le_bytes());
            attribute[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
            attribute[0x14..0x16].copy_from_slice(&(value_offset as u16).to_le_bytes());
            attribute[0x18..value_offset].copy_from_slice(&name);
            attribute[value_offset..value_offset + value.len()].copy_from_slice(value);
            self.attributes.extend(attribute);
            self
        }

        fn modified(self, unix: i64) -> Self {
            self.information(unix, 0)
        }

        /// `$STANDARD_INFORMATION`: when the file was last modified, and its attributes.
        fn information(self, unix: i64, attributes: u32) -> Self {
            let mut value = vec![0u8; 48];
            value[8..16].copy_from_slice(&filetime(unix).to_le_bytes());
            value[0x20..0x24].copy_from_slice(&attributes.to_le_bytes());
            self.resident(STANDARD_INFORMATION, "", &value)
        }

        fn name_in(self, parent: u64, name: &str, namespace: u8) -> Self {
            let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let mut value = vec![0u8; 0x42 + units.len()];
            value[0..8].copy_from_slice(&parent.to_le_bytes());
            value[0x40] = (units.len() / 2) as u8;
            value[0x41] = namespace;
            value[0x42..].copy_from_slice(&units);
            self.resident(FILE_NAME, "", &value)
        }

        fn name(self, parent: u64, name: &str) -> Self {
            self.name_in(parent, name, 1)
        }

        fn data(self, size: usize) -> Self {
            self.resident(DATA, "", &vec![7u8; size])
        }

        fn large_data(mut self, lowest_vcn: u64, allocated: u64, size: u64) -> Self {
            let mut attribute = vec![0u8; 0x48];
            attribute[0..4].copy_from_slice(&DATA.to_le_bytes());
            attribute[4..8].copy_from_slice(&0x48u32.to_le_bytes());
            attribute[8] = 1;
            attribute[0x10..0x18].copy_from_slice(&lowest_vcn.to_le_bytes());
            attribute[0x28..0x30].copy_from_slice(&allocated.to_le_bytes());
            attribute[0x30..0x38].copy_from_slice(&size.to_le_bytes());
            attribute[0x38..0x40].copy_from_slice(&size.to_le_bytes());
            self.attributes.extend(attribute);
            self
        }

        /// A sparse unnamed stream of which `on_disk` bytes of `allocated` are on the disk.
        fn sparse_data(mut self, allocated: u64, on_disk: u64, size: u64) -> Self {
            self = self.large_data(0, allocated, size);
            let at = self.attributes.len() - 0x48;
            self.attributes[at + 0x0C..at + 0x0E].copy_from_slice(&0x8000u16.to_le_bytes());
            self.attributes[at + 0x40..at + 0x48].copy_from_slice(&on_disk.to_le_bytes());
            self
        }

        fn reparse(self, tag: u32) -> Self {
            let mut value = vec![0u8; 8];
            value[0..4].copy_from_slice(&tag.to_le_bytes());
            self.resident(REPARSE_POINT, "", &value)
        }

        fn build(self) -> Vec<u8> {
            let mut record = vec![0u8; RECORD_SIZE];
            record[0..4].copy_from_slice(b"FILE");
            record[4..6].copy_from_slice(&0x30u16.to_le_bytes());
            record[6..8].copy_from_slice(&3u16.to_le_bytes());
            record[0x10..0x12].copy_from_slice(&self.sequence.to_le_bytes());
            record[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
            record[0x16..0x18].copy_from_slice(&self.flags.to_le_bytes());
            record[0x20..0x28].copy_from_slice(&self.base.to_le_bytes());
            let end = 0x38 + self.attributes.len();
            record[0x38..end].copy_from_slice(&self.attributes);
            record[end..end + 4].copy_from_slice(&END_OF_ATTRIBUTES.to_le_bytes());
            record[0x18..0x1C].copy_from_slice(&((end + 8) as u32).to_le_bytes());
            // Swap each stride's last two bytes for the update sequence number, keeping them in the
            // fixup array, as NTFS does before writing.
            let usn = [0x34, 0x12];
            record[0x30..0x32].copy_from_slice(&usn);
            for i in 1..=2 {
                let stride_end = i * FIXUP_STRIDE;
                let original = [record[stride_end - 2], record[stride_end - 1]];
                record[0x30 + 2 * i..0x32 + 2 * i].copy_from_slice(&original);
                record[stride_end - 2..stride_end].copy_from_slice(&usn);
            }
            record
        }
    }

    fn parsed(builder: Builder) -> Option<Record> {
        parse_record(&mut builder.build())
    }

    fn base(builder: Builder) -> FileRecord {
        match parsed(builder) {
            Some(Record::Base(file)) => file,
            other => panic!("expected a base record, got {other:?}"),
        }
    }

    #[test]
    fn records_are_read_with_their_fixups_undone() {
        // Padding that puts the name across the end of the first 512-byte stride, where a fixup sits.
        let file = base(
            Builder::file(3)
                .modified(1_700_000_000)
                .resident(0x40, "", &[0u8; 328])
                .name(reference(40, 2), "abcdefghijklmnop")
                .data(10),
        );
        assert_eq!(file.names, vec![(reference(40, 2), "abcdefghijklmnop".into())]);
        assert_eq!(
            (file.is_dir, file.sequence, file.size, file.mtime),
            (false, 3, 10, 1_700_000_000)
        );
    }

    #[test]
    fn torn_unused_and_foreign_records_are_skipped() {
        let mut torn = Builder::file(1).name(reference(ROOT_FOLDER, 5), "a").build();
        torn[RECORD_SIZE - 1] ^= 0xFF;
        assert!(is_torn(&torn));
        assert_eq!(parse_record(&mut torn), None);
        assert!(!is_torn(&Builder::file(1).name(reference(ROOT_FOLDER, 5), "a").build()));
        assert_eq!(parsed(Builder::unused().name(reference(ROOT_FOLDER, 5), "a")), None);
        let mut not_a_record = vec![0u8; RECORD_SIZE];
        assert_eq!(parse_record(&mut not_a_record), None);
    }

    #[test]
    fn short_dos_names_and_named_streams_are_left_out() {
        let file = base(
            Builder::file(1)
                .name_in(reference(ROOT_FOLDER, 5), "LONGFI~1.TXT", DOS_NAMESPACE)
                .name(reference(ROOT_FOLDER, 5), "long file name.txt")
                .resident(DATA, "Zone.Identifier", &[1u8; 30])
                .data(5),
        );
        assert_eq!(
            file.names,
            vec![(reference(ROOT_FOLDER, 5), "long file name.txt".into())]
        );
        assert_eq!(file.size, 5);
    }

    #[test]
    fn a_large_stream_takes_its_sizes_from_its_first_extent() {
        let file = base(Builder::file(1).large_data(0, 8192, 5000));
        assert_eq!((file.has_data, file.size, file.allocated), (true, 5000, 8192));
        let later_extent = base(Builder::file(1).large_data(16, 8192, 5000));
        assert!(!later_extent.has_data);
        assert_eq!(
            parsed(Builder::extension_of(reference(47, 1)).large_data(0, 12288, 10_000)),
            Some(Record::Extension {
                base: reference(47, 1),
                attributes: FileRecord {
                    sequence: 1,
                    has_data: true,
                    size: 10_000,
                    allocated: 12288,
                    ..FileRecord::default()
                }
            })
        );
    }

    /// The MFT from these records, numbered as given.
    fn mft(records: Vec<(usize, Builder)>) -> Mft {
        let mut mft = Mft::new(64);
        for (number, builder) in records {
            if let Some(record) = parsed(builder) {
                mft.insert(number, record);
            }
        }
        mft
    }

    /// A small volume: the root, metadata NTFS hides, two folders, and files of every kind the
    /// folder walk treats specially.
    fn volume() -> Vec<(usize, Builder)> {
        let root = reference(ROOT_FOLDER, 5);
        let users = reference(20, 1);
        let ville = reference(21, 1);
        vec![
            (ROOT_FOLDER, Builder::folder(5).name(root, ".")),
            (0, Builder::file(1).name(root, "$MFT").large_data(0, 1 << 20, 1 << 20)),
            (11, Builder::folder(11).name(root, "$Extend")),
            (30, Builder::file(1).name(reference(11, 11), "$UsnJrnl").data(100)),
            (20, Builder::folder(1).name(root, "Users")),
            (21, Builder::folder(1).name(users, "ville").modified(5)),
            (22, Builder::folder(1).name(root, "Junction").reparse(JUNCTION_TAG)),
            (40, Builder::file(1).name(ville, "a.txt").data(100).modified(1000)),
            (41, Builder::file(1).name(ville, "empty").modified(2000)),
            (
                42,
                Builder::file(1)
                    .name(users, "link1")
                    .name(ville, "link2")
                    .data(50)
                    .modified(10),
            ),
            (
                43,
                Builder::file(1)
                    .name(ville, "symlink")
                    .reparse(SYMLINK_TAG)
                    .modified(3000),
            ),
            (
                44,
                Builder::file(1)
                    .name(ville, "cloud")
                    .reparse(CLOUD_TAG_3)
                    .sparse_data(4096, 0, 500)
                    .modified(1500),
            ),
            (
                50,
                Builder::file(1)
                    .information(1200, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
                    .name(ville, "partly downloaded")
                    .reparse(CLOUD_TAG_3)
                    .sparse_data(8192, 4096, 6000),
            ),
            (45, Builder::file(1).name(reference(20, 99), "orphan").data(70)),
            (46, Builder::extension_of(reference(47, 1)).large_data(0, 12288, 10_000)),
            // Written for an earlier file in record 47.
            (49, Builder::extension_of(reference(47, 9)).large_data(0, 4096, 99)),
            (
                47,
                Builder::file(1)
                    .name(ville, "big.bin")
                    .large_data(16, 12288, 10_000)
                    .modified(20),
            ),
            (
                48,
                Builder::file(1).name(root, "pagefile.sys").large_data(0, 4096, 4096),
            ),
        ]
    }

    /// What `send_scan` sends for the folder `root_number`.
    fn scan(mut mft: Mft, root: &Path, root_number: usize, root_sequence: u16) -> (bool, Vec<ScanEvent>) {
        let (tx, rx) = mpsc::channel();
        let sent = mft.send_scan(root, reference(root_number, root_sequence), VOLUME, &tx, |_| None);
        drop(tx);
        (sent, rx.iter().collect())
    }

    type Folder = (u64, u64, String, i64, Vec<(String, u64)>);

    /// The folders sent, which is all `send_scan` sends.
    fn folders(events: &[ScanEvent]) -> Vec<Folder> {
        events
            .iter()
            .map(|event| match event {
                ScanEvent::Dir {
                    id,
                    parent,
                    name,
                    mtime,
                    files,
                } => (
                    *id,
                    *parent,
                    name.clone(),
                    *mtime,
                    files.iter().map(|file| (file.name.clone(), file.size)).collect(),
                ),
                other => panic!("only folders are sent, not {other:?}"),
            })
            .collect()
    }

    fn id(number: usize, sequence: u16) -> u64 {
        node_id(VOLUME, reference(number, sequence))
    }

    #[test]
    fn a_volume_is_sent_as_the_folder_walk_sees_it() {
        let root = PathBuf::from(r"C:\");
        let (sent, events) = scan(mft(volume()), &root, ROOT_FOLDER, 5);
        assert!(sent);
        // Not $Extend's tree, NTFS's metafiles, the junction or the orphan. The hardlink counts once,
        // in the first of its folders. The symlink counts nothing and doesn't make its folder newer;
        // the dehydrated placeholders (one with nothing on the disk, one partly downloaded) count
        // nothing but, like the empty file, do. The big file's sizes come from its extension record, not a stale one.
        assert_eq!(
            folders(&events),
            vec![
                (
                    id(ROOT_FOLDER, 5),
                    0,
                    root_name(&root),
                    0,
                    vec![("pagefile.sys".to_owned(), 4096)]
                ),
                (
                    id(20, 1),
                    id(ROOT_FOLDER, 5),
                    "Users".to_owned(),
                    10,
                    vec![("link1".to_owned(), 50)]
                ),
                (
                    id(21, 1),
                    id(20, 1),
                    "ville".to_owned(),
                    2000,
                    vec![("a.txt".to_owned(), 100), ("big.bin".to_owned(), 10_000)]
                ),
            ]
        );
    }

    #[test]
    fn a_folder_scan_counts_a_hardlink_inside_it() {
        let root = PathBuf::from(r"C:\Users\ville");
        let (sent, events) = scan(mft(volume()), &root, 21, 1);
        assert!(sent);
        assert_eq!(
            folders(&events),
            vec![(
                id(21, 1),
                0,
                "ville".to_owned(),
                2000,
                vec![
                    ("a.txt".to_owned(), 100),
                    ("link2".to_owned(), 50),
                    ("big.bin".to_owned(), 10_000)
                ]
            )]
        );
    }

    #[test]
    fn a_file_named_in_its_extension_records_goes_by_their_order() {
        let root = PathBuf::from(r"C:\");
        let volume = reference(ROOT_FOLDER, 5);
        // All its names in extension records, gathered out of order: it goes in the first one's folder.
        let records = vec![
            (ROOT_FOLDER, Builder::folder(5).name(volume, ".")),
            (20, Builder::folder(1).name(volume, "a")),
            (21, Builder::folder(1).name(volume, "b")),
            (30, Builder::file(1).data(10)),
            (
                32,
                Builder::extension_of(reference(30, 1)).name(reference(21, 1), "in b"),
            ),
            (
                31,
                Builder::extension_of(reference(30, 1)).name(reference(20, 1), "in a"),
            ),
        ];
        let (sent, events) = scan(mft(records), &root, ROOT_FOLDER, 5);
        assert!(sent);
        assert_eq!(
            folders(&events),
            vec![
                (id(ROOT_FOLDER, 5), 0, root_name(&root), 0, vec![]),
                (
                    id(20, 1),
                    id(ROOT_FOLDER, 5),
                    "a".to_owned(),
                    0,
                    vec![("in a".to_owned(), 10)]
                ),
                (id(21, 1), id(ROOT_FOLDER, 5), "b".to_owned(), 0, vec![]),
            ]
        );
    }

    #[test]
    fn a_reparse_point_whose_tag_is_not_in_its_records_is_asked_for_it() {
        let root = PathBuf::from(r"C:\");
        let volume = reference(ROOT_FOLDER, 5);
        // Reparse points whose `$REPARSE_POINT` didn't fit in their records: a symlink, as asking
        // tells, and one whose tag can't be had, which counts as the plain file it seems.
        let records = vec![
            (ROOT_FOLDER, Builder::folder(5).name(volume, ".")),
            (
                30,
                Builder::file(1)
                    .information(3000, FILE_ATTRIBUTE_REPARSE_POINT)
                    .name(volume, "link"),
            ),
            (
                31,
                Builder::file(1)
                    .information(1000, FILE_ATTRIBUTE_REPARSE_POINT)
                    .name(volume, "unknown")
                    .data(5),
            ),
            (32, Builder::file(1).name(volume, "plain").data(7)),
        ];
        let mut mft = mft(records);
        let (tx, rx) = mpsc::channel();
        let mut asked = Vec::new();
        let sent = mft.send_scan(&root, volume, VOLUME, &tx, |file| {
            asked.push(file);
            (file == reference(30, 1)).then_some(SYMLINK_TAG)
        });
        drop(tx);
        assert!(sent);
        assert_eq!(asked, vec![reference(30, 1), reference(31, 1)]);
        assert_eq!(
            folders(&rx.iter().collect::<Vec<_>>()),
            vec![(
                id(ROOT_FOLDER, 5),
                0,
                root_name(&root),
                1000,
                vec![("unknown".to_owned(), 5), ("plain".to_owned(), 7)]
            )]
        );
    }

    #[test]
    fn nothing_is_sent_for_a_root_the_mft_does_not_have() {
        for (number, sequence) in [(21, 7), (40, 1), (63, 1)] {
            let (sent, events) = scan(mft(volume()), Path::new(r"C:\x"), number, sequence);
            assert!(!sent);
            assert!(events.is_empty());
        }
    }
}
