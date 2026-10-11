#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use humansize::{BINARY, format_size};
use rsdirstat_core::scan::{ScanEvent, ScanOptions, ScannedFile};
use rsdirstat_core::tree::DirTree;

#[cfg(target_os = "linux")]
use rsdirstat_linux as scanner;
#[cfg(target_os = "macos")]
use rsdirstat_macos as scanner;
#[cfg(target_os = "windows")]
use rsdirstat_windows as scanner;

#[derive(Parser)]
#[command(name = "rsdirstat-cli", version, about = "Blazing fast disk usage scanner")]
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

    /// Scan with this many threads, any number [default: one per CPU; two on Linux; 4 on macOS]
    #[arg(long)]
    threads: Option<NonZeroUsize>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let (tx, rx) = std::sync::mpsc::channel();
    let path = args.path.clone();
    let options = ScanOptions {
        cross_filesystems: args.all,
        threads: args.threads,
    };
    let handle = std::thread::spawn(move || scanner::scan::scan(&path, options, tx, Arc::default()));

    let mut tree = DirTree::new();
    // With `--files`, the largest files so far as (size, directory, name), the smallest on top to
    // be dropped first.
    let mut files = BinaryHeap::new();

    while let Ok(event) = rx.recv() {
        match event {
            // The tree only needs each directory's total, so files are kept (outside the tree, and
            // only the largest) only when listing them.
            ScanEvent::Dir {
                id,
                parent,
                name,
                mtime,
                files: dir_files,
            } => {
                let size = ScannedFile::total(&dir_files);
                if args.files {
                    for file in dir_files {
                        files.push(Reverse((file.size, id, file.name)));
                        if files.len() > args.top {
                            files.pop();
                        }
                    }
                }
                tree.insert_dir(id, parent, name, size, mtime);
            }
            // `ScanDone`, the last event, recomputes the sizes.
            event => tree.apply(event),
        }
    }

    handle.join().map_err(|_| anyhow::anyhow!("scan thread panicked"))??;
    if !tree.complete {
        eprintln!("The scan stopped early, so these results are incomplete.");
    }

    if args.files {
        // Sorted by `Reverse`, so largest first.
        print_entries(
            files
                .into_sorted_vec()
                .into_iter()
                .map(|Reverse((size, parent, name))| (tree.full_path(parent).unwrap_or_default().join(name), size)),
        );
    } else {
        let mut dirs: Vec<(u64, u64)> = tree.nodes.iter().map(|(&id, node)| (id, node.size)).collect();
        top_n(&mut dirs, args.top, |&(_, size)| size);
        print_entries(
            dirs.iter()
                .map(|&(id, size)| (tree.full_path(id).unwrap_or_default(), size)),
        );
    }

    Ok(())
}

/// Keep only the `n` entries with the largest `key`, sorted largest first.
fn top_n<T>(items: &mut Vec<T>, n: usize, key: impl Fn(&T) -> u64) {
    let n = n.min(items.len());
    if n == 0 {
        items.clear();
        return;
    }
    items.select_nth_unstable_by_key(n - 1, |item| Reverse(key(item)));
    items.truncate(n);
    items.sort_unstable_by_key(|item| Reverse(key(item)));
}

fn print_entries(entries: impl Iterator<Item = (PathBuf, u64)>) {
    let rows: Vec<(String, PathBuf)> = entries.map(|(path, size)| (format_size(size, BINARY), path)).collect();
    let width = rows.iter().map(|(size, _)| size.len()).max().unwrap_or(0);
    for (size, path) in &rows {
        println!("{size:>width$}  {}", path.display());
    }
}
