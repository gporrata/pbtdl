mod download;
mod search;

use anyhow::{bail, Result};
use clap::Parser;
use console::style;
use dialoguer::{theme::ColorfulTheme, Select};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "pbtdl",
    about = "Search The Pirate Bay and download the best torrent",
    version
)]
struct Cli {
    /// Search query
    query: String,

    /// Directory to download files into
    #[arg(short, long, default_value = ".")]
    output: PathBuf,

    /// Number of results to show for selection
    #[arg(short, long, default_value_t = 10)]
    results: usize,

    /// Skip interactive selection and download the top result automatically
    #[arg(long)]
    auto: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    println!("{} {}", style("Searching for:").bold(), style(&cli.query).cyan());

    let torrents = search::search(&cli.query).await?;

    if torrents.is_empty() {
        bail!("No results found for {:?}", cli.query);
    }

    let top: Vec<_> = torrents.into_iter().take(cli.results).collect();

    let chosen = if cli.auto || top.len() == 1 {
        &top[0]
    } else {
        let items: Vec<String> = top
            .iter()
            .map(|t| {
                format!(
                    "{:<60} {:>6} seeders  {}",
                    truncate(&t.name, 60),
                    t.seeders,
                    t.size_human()
                )
            })
            .collect();

        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Select a torrent")
            .items(&items)
            .default(0)
            .interact()?;

        &top[selection]
    };

    println!(
        "{} {} ({} seeders, {})",
        style("Downloading:").bold().green(),
        style(&chosen.name).cyan(),
        chosen.seeders,
        chosen.size_human()
    );

    let magnet = chosen.magnet();
    download::download(&magnet, &cli.output)?;

    Ok(())
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}
