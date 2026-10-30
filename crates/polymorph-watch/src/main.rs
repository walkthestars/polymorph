use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "polymorph-watch",
    about = "Read-only terminal viewer for a Polymorph drive event ledger"
)]
struct Args {
    /// Drive event ledger written by `review drive --events-json`.
    #[arg(long)]
    events: PathBuf,
    /// Re-read the ledger when it changes (watch a live drive).
    #[arg(long)]
    follow: bool,
    /// Poll interval in milliseconds.
    #[arg(long, default_value_t = 250)]
    interval_ms: u64,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    polymorph_watch::run(args.events, args.follow, args.interval_ms)
}
