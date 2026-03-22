mod format;
mod scan;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use crate::format::human_size;

#[derive(Parser)]
#[command(
    name = "rsdirstat",
    about = "Blazing fast disk usage scanner for macOS"
)]
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
    let result = scan::scan(&args.path, args.files, args.all, args.top)?;

    let mut entries: Vec<(PathBuf, u64)> = if args.files {
        result.file_entries
    } else {
        result.dir_sizes.into_iter().collect()
    };

    entries.sort_unstable_by(|a, b| b.1.cmp(&a.1));

    let formatted: Vec<(String, &PathBuf)> = entries
        .iter()
        .map(|(path, size)| (human_size(*size), path))
        .collect();

    let max_width = formatted.iter().map(|(s, _)| s.len()).max().unwrap_or(0);

    for (size_str, path) in &formatted {
        println!("{size_str:>width$}  {}", path.display(), width = max_width);
    }

    Ok(())
}
