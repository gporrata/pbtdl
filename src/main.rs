use clap::Parser;
use pbtdl::cli::Cli;
use pbtdl::config::{ConfigEnvironment, ConfigPaths, load_or_create};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let environment = ConfigEnvironment::current();
    let paths = ConfigPaths::resolve(cli.config.as_deref(), &environment)?;
    let mut loaded = load_or_create(paths)?;
    loaded.config.apply_cli(&cli);
    Ok(())
}
