use clap::Parser;
use pbtdl::cli::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();
    Ok(())
}
