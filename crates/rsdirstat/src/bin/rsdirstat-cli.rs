#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::cmp::Reverse;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use humansize::{BINARY, format_size};
use rsdirstat_core::scan::ScanEvent;
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
}

fn main() -> Result<()> {
    let args = Args::parse();
    let root = std::fs::canonicalize(&args.path)?;

    let (tx, rx) = std::sync::mpsc::channel();
    let scan_root = root.clone();
    let handle = std::thread::spawn(move || scanner::scan::scan(&scan_root, args.all, tx));

    let mut tree = DirTree::new();
    let mut files: Vec<(u64, String, u64)> = Vec::new();

    while let Ok(event) = rx.recv() {
        match event {
            // Directory sizes come from the `Dir` events alone, so files are only kept (outside
            // the tree) when listing them.
            ScanEvent::File { parent, name, size, .. } => {
                if args.files {
                    files.push((parent, name, size));
                }
            }
            ScanEvent::ScanDone => break,
            event => tree.apply(event),
        }
    }

    handle.join().map_err(|_| anyhow::anyhow!("scan thread panicked"))??;
    tree.recompute_sizes();

    if args.files {
        top_n(&mut files, args.top, |&(_, _, size)| size);
        print_entries(
            files
                .iter()
                .map(|(parent, name, size)| (tree.full_path(*parent, &root).unwrap_or_default().join(name), *size)),
        );
    } else {
        let mut dirs: Vec<(u64, u64)> = tree.nodes.iter().map(|(&id, node)| (id, node.size)).collect();
        top_n(&mut dirs, args.top, |&(_, size)| size);
        print_entries(
            dirs.iter()
                .map(|&(id, size)| (tree.full_path(id, &root).unwrap_or_default(), size)),
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
