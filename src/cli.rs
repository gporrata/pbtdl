use clap::Parser;
use std::num::NonZeroUsize;
use std::path::PathBuf;

/// Search rendered Pirate Bay-compatible pages and download a selected result.
#[derive(Debug, Clone, Parser, PartialEq, Eq)]
#[command(name = "pbtdl", version, about)]
pub struct Cli {
    /// Search query.
    pub query: String,

    /// Directory into which the local torrent client downloads files.
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Maximum number of results shown for selection (must be greater than zero).
    #[arg(short, long)]
    pub results: Option<NonZeroUsize>,

    /// Select the highest-ranked result without prompting.
    #[arg(long)]
    pub auto: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_the_initial_cli_contract() {
        let cli = Cli::try_parse_from([
            "pbtdl",
            "legal test image",
            "--output",
            "/tmp/downloads",
            "--results",
            "5",
            "--auto",
        ])
        .expect("CLI should parse");

        assert_eq!(cli.query, "legal test image");
        assert_eq!(cli.output, Some(PathBuf::from("/tmp/downloads")));
        assert_eq!(cli.results.map(NonZeroUsize::get), Some(5));
        assert!(cli.auto);
    }

    #[test]
    fn rejects_a_zero_result_limit() {
        let error = Cli::try_parse_from(["pbtdl", "query", "--results", "0"])
            .expect_err("zero must be rejected");

        assert!(error.to_string().contains("non-zero type"));
    }
}
