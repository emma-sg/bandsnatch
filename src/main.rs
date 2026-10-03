mod api;
mod cmds;
mod cookies;
mod library;
mod lock;
mod state;
mod util;

#[macro_use]
extern crate log;
#[macro_use]
extern crate simple_error;

use clap::{Parser, Subcommand};
use env_logger::{Env, DEFAULT_FILTER_ENV};
use std::io::Write;

#[derive(Parser, Debug)]
#[clap(name = "bandsnatch", version, about, long_about = None)]
struct Args {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run Bandsnatch to download your collection.
    Run(cmds::run::Args),
    /// Re-download a single release, bypassing the state cache.
    Release(cmds::release::Args),
    /// Get the raw JSON of your Bandcamp collection page for debugging.
    DebugCollection(cmds::debug_collection::Args),
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // TODO: make default based on what release target
    let env = Env::default().filter_or(DEFAULT_FILTER_ENV, "bandsnatch=info");
    // Timestamps are local, so they line up with the container supervisor's
    // log lines and with the hour you configure `RUN_AT` in.
    env_logger::Builder::from_env(env)
        .format(|buf, record| {
            writeln!(
                buf,
                "[{} {:<5} {}] {}",
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z"),
                record.level(),
                record.target(),
                record.args()
            )
        })
        .init();

    let args = Args::parse();

    match args.command {
        Commands::Run(cmd_args) => cmds::run::command(cmd_args),
        Commands::Release(cmd_args) => cmds::release::command(cmd_args),
        Commands::DebugCollection(cmd_args) => cmds::debug_collection::command(cmd_args),
    }
}
