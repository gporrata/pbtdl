use clap::Parser;
use pbtdl::app;
use pbtdl::cli::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    app::run(cli).await
}
