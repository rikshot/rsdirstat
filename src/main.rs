#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod layout;
mod scan;
mod server;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use humansize::{BINARY, format_size};

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

    /// Launch web GUI with interactive treemap
    #[arg(long)]
    gui: bool,

    /// Port for the GUI server (0 = random)
    #[arg(long, default_value_t = 0)]
    port: u16,

    /// Don't auto-open the browser
    #[arg(long)]
    no_open: bool,

    /// Wait for manual start before scanning (for profiling)
    #[arg(long)]
    wait: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.gui {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(server::run_streaming(
            args.path,
            args.all,
            args.port,
            args.no_open,
            args.wait,
        ))?;
        return Ok(());
    }

    let result = scan::scan(&args.path, args.files, args.all, args.top)?;

    let mut entries: Vec<(PathBuf, u64)> = if args.files {
        result.file_entries
    } else {
        result.dir_sizes.into_iter().collect()
    };

    entries.sort_unstable_by(|a, b| b.1.cmp(&a.1));

    let formatted: Vec<(String, &PathBuf)> = entries
        .iter()
        .map(|(path, size)| (format_size(*size, BINARY), path))
        .collect();

    let max_width = formatted.iter().map(|(s, _)| s.len()).max().unwrap_or(0);

    for (size_str, path) in &formatted {
        println!("{size_str:>width$}  {}", path.display(), width = max_width);
    }

    Ok(())
}
