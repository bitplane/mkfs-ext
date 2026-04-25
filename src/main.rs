use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

mod ext;

use ext::Ext;

#[derive(Parser)]
#[command(
    name = "mkfs-ext",
    version,
    about = "Create original ext (pre-ext2) filesystem images"
)]
struct Cli {
    /// Populate from this directory tree (recursive)
    #[arg(short = 'd', long)]
    directory: Option<PathBuf>,

    /// Output filename
    filename: PathBuf,

    /// Image size in megabytes
    size_mb: u32,
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut ext = Ext::new(cli.size_mb)?;
    if let Some(src) = &cli.directory {
        ext.populate_root(src)?;
    }
    let stats = ext.write_to(&cli.filename)?;

    println!(
        "Created ext image: {} ({}MB, {} inodes, {} blocks, magic=0x{:x})",
        cli.filename.display(),
        cli.size_mb,
        stats.ninodes,
        stats.total_blocks,
        ext::MAGIC,
    );
    println!(
        "First data zone: {}, free blocks: {}, free inodes: {}",
        stats.first_data_zone, stats.free_blocks, stats.free_inodes,
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mkfs-ext: {e:#}");
            ExitCode::FAILURE
        }
    }
}
