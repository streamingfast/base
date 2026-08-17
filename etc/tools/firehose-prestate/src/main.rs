#![doc = include_str!("../README.md")]

use base_firehose_prestate::PrestateNetwork;
use clap::{Parser, Subcommand};
use eyre::Result;
use firehose_tracer_prestate::GenerateArgs;

/// Generate replayable Firehose prestate fixtures for Base transactions.
#[derive(Parser, Debug)]
#[command(name = "base-firehose-prestate")]
struct Cli {
    /// Which Base network the transaction belongs to.
    #[arg(long, value_enum, default_value = "base", global = true)]
    network: PrestateNetwork,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Write `prestate.json` for a mined transaction, read from an archive node.
    Generate(GenerateArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Generate(args) => {
            args.run(cli.network).await?;
        }
    }

    Ok(())
}
