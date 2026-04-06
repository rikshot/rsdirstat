use std::path::Path;

use anyhow::Result;
use rsdirstat_core::protocol::ScanEvent;

pub fn scan(_root: &Path, _cross_filesystems: bool, _tx: std::sync::mpsc::Sender<ScanEvent>) -> Result<()> {
    anyhow::bail!("Linux scanner not yet implemented")
}
