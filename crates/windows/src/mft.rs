//! Scanning an NTFS volume by reading its master file table (MFT) off the disk, which `ntfs` parses.
//! Reading the volume needs administrator rights. Where it can be read, this is several times faster
//! than the folder walk, whose cost is Windows' own work for every folder opened, listed and closed
//! (its file system filters above all), and it doesn't depend on what Windows has cached.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};

use anyhow::{Context, Result, anyhow, bail};
use rsdirstat_core::scan::{ScanEvent, ScanOutcome, plain_path};
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_NO_BUFFERING, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FlushFileBuffers, GetFinalPathNameByHandleW, VOLUME_NAME_GUID,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_GET_RETRIEVAL_POINTERS, NTFS_VOLUME_DATA_BUFFER,
};

use crate::ntfs::{Mft, Record, is_torn, parse_record, u32_at, u64_at};
use crate::scan::reparse_tag_by_id;

/// How much of the MFT one read takes.
const CHUNK: usize = 4 << 20;
/// Reads in flight at most: an NVMe drive needs several at once to reach its speed. On the test
/// box's, one reader was ~20% slower than two, and four as fast as eight.
const MAX_READERS: usize = 4;
/// Unbuffered reads need a buffer aligned to the disk's sectors; a page covers any sector size.
const ALIGNMENT: usize = 4096;
/// How many times a read is made while it has a record caught being rewritten.
const READ_ATTEMPTS: usize = 3;

/// Scan the folder `path` (canonical), open as `root` with file reference `root_reference` on the
/// volume with serial number `volume_serial`, from the volume's MFT, reading with up to `threads`
/// reads in flight. The whole MFT is read however small the folder. An error means the volume can't
/// be read this way (no administrator rights, not NTFS, a network share) and the folder walk should
/// scan it instead: `ScanStart` may have been sent by then, but nothing after it.
pub(crate) fn scan(
    path: &Path,
    root: &OwnedHandle,
    root_reference: u64,
    volume_serial: u32,
    threads: usize,
    tx: &Sender<ScanEvent>,
    cancel: &AtomicBool,
) -> Result<()> {
    let volume = volume_path(root)?;
    let open = |access| {
        OpenOptions::new()
            .access_mode(access)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&volume)
    };
    // Opened for writing too, where it can be, to flush it below.
    let (device, writable) = match open(GENERIC_READ | GENERIC_WRITE) {
        Ok(device) => (device, true),
        Err(_) => (
            open(GENERIC_READ).context("opening the volume (needs administrator rights)")?,
            false,
        ),
    };
    // The volume can be read: the scan has started, though nothing is known until it's all read.
    let _ = tx.send(ScanEvent::ScanStart {
        root: plain_path(path).into_owned(),
    });

    // From here on, failing is unexpected: say so, as the walk takes over.
    let fall_back = |e: anyhow::Error| {
        eprintln!(
            "Can't read the MFT for {}, walking it instead: {e:#}",
            plain_path(path).display()
        );
        e
    };

    // The MFT on the disk lags what Windows has cached by a few seconds; flushing the volume brings
    // it up to date, so files made or deleted just before the scan are seen as they are. A volume
    // that can't be written is read as it is.
    if writable {
        unsafe { FlushFileBuffers(device.as_raw_handle()) };
    }
    let mut data: NTFS_VOLUME_DATA_BUFFER = unsafe { std::mem::zeroed() };
    let data_bytes =
        unsafe { std::slice::from_raw_parts_mut(&mut data as *mut _ as *mut u8, size_of::<NTFS_VOLUME_DATA_BUFFER>()) };
    ioctl(&device, FSCTL_GET_NTFS_VOLUME_DATA, &[], data_bytes)
        .context("not an NTFS volume")
        .map_err(fall_back)?;
    drop(device);

    let cluster = data.BytesPerCluster as usize;
    let record_size = data.BytesPerFileRecordSegment as usize;
    if record_size == 0 || cluster == 0 || !cluster.is_multiple_of(record_size) || !CHUNK.is_multiple_of(cluster) {
        return Err(fall_back(anyhow!(
            "unexpected NTFS geometry: {cluster}-byte clusters, {record_size}-byte records"
        )));
    }
    let record_count = data.MftValidDataLength as usize / record_size;
    let reads = mft_reads(&volume, cluster, data.MftValidDataLength as usize).map_err(fall_back)?;

    // Each reader takes the next read, parses its records and hands them over; this thread gathers
    // them as they come. An error ends the gathering, which stops the readers.
    let next = AtomicUsize::new(0);
    let (records_tx, records_rx) = mpsc::channel::<Result<Vec<(usize, Record)>>>();
    let mut mft = std::thread::scope(|scope| -> Result<Mft> {
        for _ in 0..threads.clamp(1, MAX_READERS) {
            let records_tx = records_tx.clone();
            let (volume, reads, next) = (&volume, &reads, &next);
            scope.spawn(move || {
                if let Err(e) = read_records(volume, reads, next, record_size, cancel, &records_tx) {
                    let _ = records_tx.send(Err(e));
                }
            });
        }
        drop(records_tx);
        // Made while the first reads are under way: a big MFT's table takes a while to set up.
        let mut mft = Mft::new(record_count);
        for records in records_rx {
            for (number, record) in records? {
                mft.insert(number, record);
            }
        }
        Ok(mft)
    })
    .map_err(fall_back)?;

    let outcome = if cancel.load(Ordering::Relaxed) {
        ScanOutcome::Cancelled
    } else if mft.send_scan(path, root_reference, volume_serial as u64, tx, |file| {
        reparse_tag_by_id(root, file as i64)
    }) {
        ScanOutcome::Complete
    } else {
        return Err(fall_back(anyhow!("the scan root isn't in its volume's MFT")));
    };
    // Before the table is freed, so nothing waits for that.
    let _ = tx.send(ScanEvent::ScanDone { outcome });
    Ok(())
}

/// The volume `handle` is on, as a device path (`\\?\Volume{GUID}`): what the volume opens as,
/// whether it has a drive letter or is mounted in a folder.
fn volume_path(handle: &OwnedHandle) -> Result<String> {
    let mut buffer = vec![0u16; 32 * 1024];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            handle.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            VOLUME_NAME_GUID,
        )
    } as usize;
    if length == 0 || length >= buffer.len() {
        bail!("the scan root's volume has no GUID path (a network share?)");
    }
    let path = String::from_utf16_lossy(&buffer[..length]);
    match path.find(r"}\") {
        Some(end) if path.starts_with(r"\\?\Volume{") => Ok(path[..=end].to_owned()),
        _ => bail!("unexpected volume path {path}"),
    }
}

/// Where the MFT's first `valid` bytes lie on `volume`, as reads of up to `CHUNK` bytes: (the first
/// record they hold, where on the disk, how many bytes). Each read is whole clusters.
fn mft_reads(volume: &str, cluster: usize, valid: usize) -> Result<Vec<(usize, u64, usize)>> {
    let mft = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(format!(r"{volume}\$MFT"))
        .context("opening $MFT")?;
    // Extents as (next VCN, LCN) pairs after a 16-byte header: a count, then the first extent's VCN.
    let mut pointers = vec![0u8; 1 << 20];
    ioctl(&mft, FSCTL_GET_RETRIEVAL_POINTERS, &0i64.to_le_bytes(), &mut pointers)
        .context("finding the MFT on the disk")?;
    let count = u32_at(&pointers, 0) as usize;
    let cluster = cluster as u64;
    let mut vcn = u64_at(&pointers, 8);
    let mut reads = Vec::new();
    for i in 0..count {
        let (next_vcn, lcn) = (u64_at(&pointers, 16 + 16 * i), u64_at(&pointers, 24 + 16 * i) as i64);
        if lcn >= 0 {
            let mut at = vcn;
            while at < next_vcn && ((at * cluster) as usize) < valid {
                let clusters = (next_vcn - at).min(CHUNK as u64 / cluster);
                reads.push((
                    (at * cluster) as usize,
                    (lcn as u64 + at - vcn) * cluster,
                    (clusters * cluster) as usize,
                ));
                at += clusters;
            }
        }
        vcn = next_vcn;
    }
    Ok(reads)
}

/// One reader: take reads from `reads` until none are left, the scan is cancelled or the records stop
/// being gathered, and send each read's parsed records. Its own handle to `volume`, unbuffered, so
/// the reads run side by side and don't fill Windows' cache.
fn read_records(
    volume: &str,
    reads: &[(usize, u64, usize)],
    next: &AtomicUsize,
    record_size: usize,
    cancel: &AtomicBool,
    records_tx: &Sender<Result<Vec<(usize, Record)>>>,
) -> Result<()> {
    let mut device = OpenOptions::new()
        .access_mode(GENERIC_READ)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_NO_BUFFERING)
        .open(volume)?;
    let mut raw = vec![0u8; CHUNK + ALIGNMENT];
    let start = raw.as_ptr().align_offset(ALIGNMENT);
    let buffer = &mut raw[start..start + CHUNK];
    loop {
        let index = next.fetch_add(1, Ordering::Relaxed);
        if index >= reads.len() || cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let (mft_offset, disk_offset, length) = reads[index];
        // A record NTFS is rewriting can be caught half written; read again, it won't be. One that
        // stays torn is left out.
        for _ in 0..READ_ATTEMPTS {
            device.seek(SeekFrom::Start(disk_offset))?;
            device.read_exact(&mut buffer[..length])?;
            if !buffer[..length].chunks_exact(record_size).any(is_torn) {
                break;
            }
        }
        let first = mft_offset / record_size;
        let records = buffer[..length]
            .chunks_exact_mut(record_size)
            .enumerate()
            .map(|(i, bytes)| (first + i, bytes))
            .filter_map(|(number, bytes)| Some((number, parse_record(bytes)?)))
            .collect();
        if records_tx.send(Ok(records)).is_err() {
            return Ok(());
        }
    }
}

fn ioctl(file: &File, code: u32, input: &[u8], output: &mut [u8]) -> Result<()> {
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            input.as_ptr() as *const c_void,
            input.len() as u32,
            output.as_mut_ptr() as *mut c_void,
            output.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
