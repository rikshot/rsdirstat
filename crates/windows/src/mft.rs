//! Scanning an NTFS volume by reading its master file table (MFT) off the disk, which `ntfs` parses.
//! Reading the volume needs administrator rights. Where it can be read, this is several times faster
//! than the folder walk, whose cost is Windows' own work for every folder opened, listed and closed
//! (its file system filters above all), and it doesn't depend on what Windows has cached.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};

use anyhow::{Context, Result, bail};
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

use crate::ntfs::{Mft, Record, parse_record};

/// How much of the MFT one read takes.
const CHUNK: usize = 4 << 20;
/// Reads in flight at most: an NVMe drive needs several at once to reach its speed.
const MAX_READERS: usize = 8;
/// Unbuffered reads need a buffer aligned to the disk's sectors; a page covers any sector size.
const ALIGNMENT: usize = 4096;

/// Scan the folder `path` (canonical), open as `root` with file reference `root_reference` on the
/// volume with serial number `volume_serial`, from the volume's MFT, reading with up to `threads`
/// reads in flight. An error, with nothing sent, means the volume can't be read this way (no
/// administrator rights, not NTFS, a network share) and the folder walk should scan it instead.
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
    let device = OpenOptions::new()
        .access_mode(GENERIC_READ | GENERIC_WRITE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&volume)
        .context("opening the volume (needs administrator rights)")?;
    // The MFT on the disk lags what Windows has cached by a few seconds; flushing the volume brings
    // it up to date, so files made just before the scan are in it.
    unsafe { FlushFileBuffers(device.as_raw_handle()) };
    let mut data: NTFS_VOLUME_DATA_BUFFER = unsafe { std::mem::zeroed() };
    let data_bytes =
        unsafe { std::slice::from_raw_parts_mut(&mut data as *mut _ as *mut u8, size_of::<NTFS_VOLUME_DATA_BUFFER>()) };
    ioctl(&device, FSCTL_GET_NTFS_VOLUME_DATA, &[], data_bytes).context("not an NTFS volume")?;
    drop(device);

    let cluster = data.BytesPerCluster as usize;
    let record_size = data.BytesPerFileRecordSegment as usize;
    if record_size == 0 || cluster == 0 || !cluster.is_multiple_of(record_size) || !CHUNK.is_multiple_of(cluster) {
        bail!("unexpected NTFS geometry: {cluster}-byte clusters, {record_size}-byte records");
    }
    let record_count = data.MftValidDataLength as usize / record_size;
    let reads = mft_reads(&volume, cluster, data.MftValidDataLength as usize)?;

    // Each reader takes the next read, parses its records and hands them over; this thread gathers
    // them as they come.
    let mut mft = Mft::new(record_count);
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let (records_tx, records_rx) = mpsc::channel::<Result<Vec<(usize, Record)>>>();
    let mut error = None;
    std::thread::scope(|scope| {
        for _ in 0..threads.clamp(1, MAX_READERS) {
            let records_tx = records_tx.clone();
            let (volume, reads, next, failed) = (&volume, &reads, &next, &failed);
            scope.spawn(move || {
                let result = read_records(
                    volume,
                    reads,
                    next,
                    record_size,
                    record_count,
                    cancel,
                    failed,
                    &records_tx,
                );
                if let Err(e) = result {
                    failed.store(true, Ordering::Relaxed);
                    let _ = records_tx.send(Err(e));
                }
            });
        }
        drop(records_tx);
        for batch in records_rx {
            match batch {
                Ok(records) => records
                    .into_iter()
                    .for_each(|(number, record)| mft.insert(number, record)),
                Err(e) => error = Some(e),
            }
        }
    });
    if let Some(e) = error {
        return Err(e);
    }

    if cancel.load(Ordering::Relaxed) {
        let _ = tx.send(ScanEvent::ScanStart {
            root: plain_path(path).into_owned(),
        });
        let _ = tx.send(ScanEvent::ScanDone {
            outcome: ScanOutcome::Cancelled,
        });
        return Ok(());
    }
    if !mft.send_scan(path, root_reference, volume_serial as u64, tx) {
        bail!("the scan root isn't in its volume's MFT");
    }
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
    let u64_at = |at: usize| u64::from_le_bytes(pointers[at..at + 8].try_into().unwrap());
    let count = u32::from_le_bytes(pointers[0..4].try_into().unwrap()) as usize;
    let cluster = cluster as u64;
    let mut vcn = u64_at(8);
    let mut reads = Vec::new();
    for i in 0..count {
        let (next_vcn, lcn) = (u64_at(16 + 16 * i), u64_at(24 + 16 * i) as i64);
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

/// One reader: take reads from `reads` until none are left, the scan is cancelled or another reader
/// failed, and send each read's parsed records. Its own handle to `volume`, unbuffered, so the reads
/// run side by side and don't fill Windows' cache.
#[allow(clippy::too_many_arguments)]
fn read_records(
    volume: &str,
    reads: &[(usize, u64, usize)],
    next: &AtomicUsize,
    record_size: usize,
    record_count: usize,
    cancel: &AtomicBool,
    failed: &AtomicBool,
    records_tx: &Sender<Result<Vec<(usize, Record)>>>,
) -> Result<()> {
    let device = OpenOptions::new()
        .access_mode(GENERIC_READ)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_NO_BUFFERING)
        .open(volume)?;
    let mut raw = vec![0u8; CHUNK + ALIGNMENT];
    let start = raw.as_ptr().align_offset(ALIGNMENT);
    let buffer = &mut raw[start..start + CHUNK];
    loop {
        let index = next.fetch_add(1, Ordering::Relaxed);
        if index >= reads.len() || cancel.load(Ordering::Relaxed) || failed.load(Ordering::Relaxed) {
            return Ok(());
        }
        let (mft_offset, disk_offset, length) = reads[index];
        read_exact_at(&device, &mut buffer[..length], disk_offset)?;
        let first = mft_offset / record_size;
        let records = buffer[..length]
            .chunks_exact_mut(record_size)
            .enumerate()
            .map(|(i, bytes)| (first + i, bytes))
            .take_while(|&(number, _)| number < record_count)
            .filter_map(|(number, bytes)| Some((number, parse_record(bytes)?)))
            .collect();
        if records_tx.send(Ok(records)).is_err() {
            return Ok(());
        }
    }
}

fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> Result<()> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = file.seek_read(&mut buffer[filled..], offset + filled as u64)?;
        if read == 0 {
            bail!("the volume ended at {} reading the MFT", offset + filled as u64);
        }
        filled += read;
    }
    Ok(())
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
