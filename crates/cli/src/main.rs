use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use humansize::{BINARY, format_size};
use rsdirstat_core::protocol::ScanEvent;
use rsdirstat_core::tree::DirTree;

#[cfg(target_os = "macos")]
use rsdirstat_macos as scanner;
#[cfg(target_os = "linux")]
use rsdirstat_linux as scanner;
#[cfg(target_os = "windows")]
use rsdirstat_windows as scanner;

#[derive(Parser)]
#[command(name = "rsdirstat", about = "Blazing fast disk usage scanner for macOS")]
struct Args {
    /// Path to scan
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Show individual files instead of directories
    #[arg(long)]
    files: bool,

    /// Number of results to display
    #[arg(long, default_value_t = 10)]
    top: usize,

    /// Cross filesystem boundaries
    #[arg(long)]
    all: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let root = std::fs::canonicalize(&args.path)?;

    let (tx, rx) = std::sync::mpsc::channel();
    let scan_root = root.clone();
    let handle = std::thread::spawn(move || scanner::scan::scan(&scan_root, args.all, tx));

    let mut tree = DirTree::new();
    let mut file_entries: Vec<(u64, String, u64)> = Vec::new();

    while let Ok(event) = rx.recv() {
        match event {
            ScanEvent::ScanStart { .. } => {}
            ScanEvent::Dir { id, parent, name, size, mtime } => {
                tree.insert_dir(id, parent, &name, size, mtime);
            }
            ScanEvent::File { parent, name, size, mtime } => {
                tree.insert_file(parent, &name, size, mtime);
                if args.files {
                    file_entries.push((parent, name, size));
                }
            }
            ScanEvent::ScanDone => break,
        }
    }

    handle.join().map_err(|_| anyhow::anyhow!("scan thread panicked"))??;
    tree.recompute_sizes();

    if args.files {
        let top = args.top.min(file_entries.len());
        if top > 0 {
            file_entries.select_nth_unstable_by(top - 1, |a, b| b.2.cmp(&a.2));
            file_entries.truncate(top);
            file_entries.sort_unstable_by(|a, b| b.2.cmp(&a.2));
            print_entries(file_entries.iter().map(|(parent, name, size)| {
                let mut path = tree.full_path(*parent, &root).unwrap_or_default();
                path.push(name);
                (path, *size)
            }));
        }
    } else {
        let mut dir_list: Vec<(u64, u64)> = tree.recursive_sizes.iter().map(|(&id, &size)| (id, size)).collect();
        let top = args.top.min(dir_list.len());
        if top > 0 {
            dir_list.select_nth_unstable_by(top - 1, |a, b| b.1.cmp(&a.1));
            dir_list.truncate(top);
            dir_list.sort_unstable_by(|a, b| b.1.cmp(&a.1));
            print_entries(dir_list.iter().map(|(id, size)| {
                (tree.full_path(*id, &root).unwrap_or_default(), *size)
            }));
        }
    }

    Ok(())
}

fn print_entries(entries: impl Iterator<Item = (PathBuf, u64)>) {
    let entries: Vec<_> = entries.collect();
    let formatted: Vec<(String, &PathBuf)> = entries
        .iter()
        .map(|(path, size)| (format_size(*size, BINARY), path))
        .collect();

    let max_width = formatted.iter().map(|(s, _)| s.len()).max().unwrap_or(0);

    for (size_str, path) in &formatted {
        println!("{size_str:>width$}  {}", path.display(), width = max_width);
    }
}
