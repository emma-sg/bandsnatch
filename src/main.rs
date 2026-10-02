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
    // TODO: custom format
    // TODO: make default based on what release target
    let env = Env::default().filter_or(DEFAULT_FILTER_ENV, "bandsnatch=info");
    env_logger::init_from_env(env);

    let args = Args::parse();

    match args.command {
        Commands::Run(cmd_args) => cmds::run::command(cmd_args),
        Commands::Release(cmd_args) => cmds::release::command(cmd_args),
        Commands::DebugCollection(cmd_args) => cmds::debug_collection::command(cmd_args),
    }
}
