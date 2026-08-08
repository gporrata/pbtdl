//! End-to-end CLI orchestration over replaceable discovery, search, selection, and download services.

use crate::browser::{BrowserSession, NavigationPolicy};
use crate::cli::Cli;
use crate::config::{AppConfig, ConfigEnvironment, ConfigPaths, DiscoveryConfig, load_or_create};
use crate::discovery::{DiscoveryEngine, DiscoveryError, ValidatedCandidate};
use crate::model::{MagnetUri, TorrentResult};
use crate::search::{SearchEngine, SearchError};
use crate::selection::{ResultChooser, TerminalChooser, format_result};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use console::style;
use std::path::Path;
use std::time::Duration;

#[async_trait]
pub trait WorkflowProvider: Send {
    async fn discover(&mut self) -> Result<Vec<ValidatedCandidate>>;
    async fn search(
        &mut self,
        candidates: &[ValidatedCandidate],
        query: &str,
    ) -> Result<Vec<TorrentResult>>;
}

#[async_trait]
pub trait DownloadService: Send {
    async fn download(&mut self, magnet: &MagnetUri, output_directory: &Path) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub selected: TorrentResult,
    pub downloaded: bool,
}

pub async fn run(cli: Cli) -> Result<()> {
    run_with_policy(cli, NavigationPolicy::production()).await
}

async fn run_with_policy(cli: Cli, policy: NavigationPolicy) -> Result<()> {
    let environment = ConfigEnvironment::current();
    let paths = ConfigPaths::resolve(cli.config.as_deref(), &environment)?;
    let mut loaded = load_or_create(paths)?;
    loaded.config.apply_cli(&cli);
    if loaded.created {
        eprintln!(
            "{} {}",
            style("Created configuration:").bold(),
            loaded.paths.config_file.display()
        );
    }

    eprintln!("{}", style("Starting isolated Chromium...").bold());
    let mut browser = BrowserSession::launch(&loaded.config.browser, policy)
        .await
        .context("browser stage failed")?;
    let result_timeout = Duration::from_secs(loaded.config.browser.selector_timeout_seconds);
    let mut provider = BrowserWorkflowProvider {
        browser: &mut browser,
        discovery_engine: DiscoveryEngine::with_policy(&loaded.paths.cache_dir, policy),
        discovery_settings: loaded.config.discovery.clone(),
        search_engine: SearchEngine::new(result_timeout),
        diagnostics: Vec::new(),
    };

    eprintln!("{}", style("Discovering rendered proxy pages...").bold());
    let results = collect_results(&mut provider, &cli.query).await;
    let diagnostics = std::mem::take(&mut provider.diagnostics);
    drop(provider);
    let shutdown = browser.shutdown().await;
    let results = results?;
    shutdown.context("browser shutdown failed")?;
    for diagnostic in diagnostics {
        eprintln!("{diagnostic}");
    }

    let mut chooser = TerminalChooser::new(loaded.config.search.max_title_characters.get());
    let mut downloader = UnavailableDownloader;
    let outcome =
        complete_results(results, &loaded.config, &cli, &mut chooser, &mut downloader).await?;
    if outcome.downloaded {
        println!("{}", style("Download completed.").bold().green());
    }
    Ok(())
}

pub async fn collect_results(
    provider: &mut dyn WorkflowProvider,
    query: &str,
) -> Result<Vec<TorrentResult>> {
    let candidates = provider.discover().await?;
    if candidates.is_empty() {
        bail!("discovery returned no validated candidates");
    }
    provider.search(&candidates, query).await
}

pub async fn complete_results(
    results: Vec<TorrentResult>,
    config: &AppConfig,
    cli: &Cli,
    chooser: &mut dyn ResultChooser,
    downloader: &mut dyn DownloadService,
) -> Result<RunOutcome> {
    if results.is_empty() {
        bail!("no torrent results were found for {:?}", cli.query);
    }
    let limited: Vec<_> = results
        .into_iter()
        .take(config.search.result_limit.get())
        .collect();
    if limited.is_empty() {
        bail!("no eligible torrent results remain after applying the result limit");
    }
    let selected_index = chooser.choose(&limited, cli.auto)?;
    let selected = limited
        .get(selected_index)
        .cloned()
        .ok_or_else(|| anyhow!("selection returned an out-of-range result index"))?;
    println!(
        "{} {}",
        style(if cli.dry_run {
            "Selected (dry run):"
        } else {
            "Selected:"
        })
        .bold()
        .green(),
        format_result(&selected, config.search.max_title_characters.get())
    );

    if cli.dry_run {
        return Ok(RunOutcome {
            selected,
            downloaded: false,
        });
    }
    downloader
        .download(&selected.magnet, &config.downloader.output_directory)
        .await?;
    Ok(RunOutcome {
        selected,
        downloaded: true,
    })
}

struct BrowserWorkflowProvider<'a> {
    browser: &'a mut BrowserSession,
    discovery_engine: DiscoveryEngine,
    discovery_settings: DiscoveryConfig,
    search_engine: SearchEngine,
    diagnostics: Vec<String>,
}

#[async_trait]
impl WorkflowProvider for BrowserWorkflowProvider<'_> {
    async fn discover(&mut self) -> Result<Vec<ValidatedCandidate>> {
        let outcome = self
            .discovery_engine
            .discover_and_validate(self.browser, &self.discovery_settings)
            .await
            .map_err(discovery_error)?;
        if !outcome.failures.is_empty() {
            self.diagnostics.push(format!(
                "Discovery skipped {} source/candidate attempt(s): {}",
                outcome.failures.len(),
                outcome
                    .failures
                    .iter()
                    .take(3)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        if let Some(warning) = outcome.cache_warning {
            self.diagnostics.push(format!("Cache warning: {warning}"));
        }
        Ok(outcome.candidates)
    }

    async fn search(
        &mut self,
        candidates: &[ValidatedCandidate],
        query: &str,
    ) -> Result<Vec<TorrentResult>> {
        eprintln!("{}", style("Searching rendered candidate pages...").bold());
        let outcome = self
            .search_engine
            .search_candidates(self.browser, candidates, query)
            .await
            .map_err(search_error)?;
        if !outcome.failures.is_empty() {
            self.diagnostics.push(format!(
                "Search skipped {} candidate(s): {}",
                outcome.failures.len(),
                outcome
                    .failures
                    .iter()
                    .take(3)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        Ok(outcome.results)
    }
}

fn discovery_error(error: DiscoveryError) -> anyhow::Error {
    let summary = error
        .failures()
        .iter()
        .take(5)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    if summary.is_empty() {
        anyhow!(error)
    } else {
        anyhow!("{error}: {summary}")
    }
}

fn search_error(error: SearchError) -> anyhow::Error {
    let summary = error
        .failures
        .iter()
        .take(5)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    if summary.is_empty() {
        anyhow!(error)
    } else {
        anyhow!("{error}: {summary}")
    }
}

struct UnavailableDownloader;

#[async_trait]
impl DownloadService for UnavailableDownloader {
    async fn download(&mut self, _magnet: &MagnetUri, _output_directory: &Path) -> Result<()> {
        bail!("local downloader adapters are not available yet; rerun with --dry-run")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{FormMethod, QueryInput, normalize_candidate};
    use crate::model::MagnetUri;
    use clap::Parser;
    use std::io;
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    fn torrent(name: &str, hash: &str, seeders: u64) -> TorrentResult {
        TorrentResult {
            name: name.to_string(),
            magnet: MagnetUri::from_str(&format!("magnet:?xt=urn:btih:{hash}&dn={name}"))
                .expect("magnet"),
            seeders: Some(seeders),
            leechers: Some(1),
            size_bytes: Some(1024),
            category: Some("Legal".to_string()),
            source_host: "proxy.example".to_string(),
        }
    }

    fn candidate() -> ValidatedCandidate {
        let url = url::Url::parse("https://93.184.216.30/").expect("candidate URL");
        ValidatedCandidate {
            candidate: normalize_candidate(url.clone()).expect("candidate"),
            rendered_url: url.clone(),
            search_form: crate::discovery::SearchForm {
                form_index: 0,
                input: QueryInput {
                    input_index: 0,
                    name: Some("q".to_string()),
                    id: None,
                },
                action: url,
                method: FormMethod::Get,
            },
        }
    }

    struct MockProvider {
        candidates: Vec<ValidatedCandidate>,
        results: Vec<TorrentResult>,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl WorkflowProvider for MockProvider {
        async fn discover(&mut self) -> Result<Vec<ValidatedCandidate>> {
            self.calls.lock().expect("call lock").push("discover");
            Ok(self.candidates.clone())
        }

        async fn search(
            &mut self,
            _candidates: &[ValidatedCandidate],
            _query: &str,
        ) -> Result<Vec<TorrentResult>> {
            self.calls.lock().expect("call lock").push("search");
            Ok(self.results.clone())
        }
    }

    #[tokio::test]
    async fn orchestration_calls_mock_discovery_before_mock_search() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let expected = torrent("top", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 50);
        let mut provider = MockProvider {
            candidates: vec![candidate()],
            results: vec![expected.clone()],
            calls: Arc::clone(&calls),
        };

        let results = collect_results(&mut provider, "query")
            .await
            .expect("collect results");

        assert_eq!(results, vec![expected]);
        assert_eq!(
            calls.lock().expect("call lock").as_slice(),
            ["discover", "search"]
        );
    }

    struct FixedChooser(usize);

    impl ResultChooser for FixedChooser {
        fn choose(&mut self, _results: &[TorrentResult], automatic: bool) -> Result<usize> {
            assert!(automatic);
            Ok(self.0)
        }
    }

    struct RecordingDownloader {
        calls: usize,
        output: Option<PathBuf>,
    }

    #[async_trait]
    impl DownloadService for RecordingDownloader {
        async fn download(&mut self, _magnet: &MagnetUri, output_directory: &Path) -> Result<()> {
            self.calls += 1;
            self.output = Some(output_directory.to_path_buf());
            Ok(())
        }
    }

    #[tokio::test]
    async fn dry_run_applies_limit_selects_top_and_never_downloads() {
        let cli = Cli::try_parse_from(["pbtdl", "query", "--auto", "--dry-run", "--results", "1"])
            .expect("CLI");
        let mut config = AppConfig::default();
        config.apply_cli(&cli);
        let top = torrent("top", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 100);
        let lower = torrent("lower", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 10);
        let mut chooser = FixedChooser(0);
        let mut downloader = RecordingDownloader {
            calls: 0,
            output: None,
        };

        let outcome = complete_results(
            vec![top.clone(), lower],
            &config,
            &cli,
            &mut chooser,
            &mut downloader,
        )
        .await
        .expect("dry run");

        assert_eq!(outcome.selected, top);
        assert!(!outcome.downloaded);
        assert_eq!(downloader.calls, 0);
    }

    #[tokio::test]
    async fn empty_results_return_clear_error() {
        let cli = Cli::try_parse_from(["pbtdl", "query", "--auto", "--dry-run"]).expect("CLI");
        let mut chooser = FixedChooser(0);
        let mut downloader = RecordingDownloader {
            calls: 0,
            output: None,
        };

        let error = complete_results(
            Vec::new(),
            &AppConfig::default(),
            &cli,
            &mut chooser,
            &mut downloader,
        )
        .await
        .expect_err("empty results must fail");

        assert!(error.to_string().contains("no torrent results"));
        assert_eq!(downloader.calls, 0);
    }

    async fn serve(
        body_for_path: Arc<dyn Fn(&str) -> String + Send + Sync>,
    ) -> (url::Url, JoinHandle<io::Result<()>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind local server");
        let address = listener.local_addr().expect("server address");
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await?;
                let body_for_path = Arc::clone(&body_for_path);
                tokio::spawn(async move {
                    let mut buffer = [0_u8; 8192];
                    let count = stream.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..count]);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let body = body_for_path(path);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (
            url::Url::parse(&format!("http://{address}/")).expect("local URL"),
            task,
        )
    }

    #[tokio::test]
    #[ignore = "requires a system Chrome/Chromium executable"]
    async fn local_fixture_cli_dry_run_completes_without_downloader() {
        if crate::browser::locate_browser(None).is_err() {
            eprintln!("skipping: no system Chrome or Chromium executable");
            return;
        }
        let candidate_handler: Arc<dyn Fn(&str) -> String + Send + Sync> = Arc::new(|path| {
            if path.starts_with("/search?") {
                include_str!("../tests/fixtures/search/classic.html").to_string()
            } else {
                include_str!("../tests/fixtures/discovery/valid_candidate.html").to_string()
            }
        });
        let (candidate_url, candidate_server) = serve(candidate_handler).await;
        let source_body =
            format!("<!doctype html><a href=\"{candidate_url}\">TPB proxy local fixture</a>");
        let source_handler: Arc<dyn Fn(&str) -> String + Send + Sync> =
            Arc::new(move |_| source_body.clone());
        let (source_url, source_server) = serve(source_handler).await;
        let temp = tempfile::tempdir().expect("temporary directory");
        let config_path = temp.path().join("pbtdl.toml");
        let config = format!(
            r#"schema_version = 1
[discovery]
source_pages = ["{source_url}"]
seed_candidates = []
max_source_pages = 1
max_candidates = 4
cache_ttl_seconds = 60

[browser]
headless = true
navigation_timeout_seconds = 10
selector_timeout_seconds = 10
overall_timeout_seconds = 30
"#
        );
        std::fs::write(&config_path, config).expect("write test configuration");
        let cli = Cli::try_parse_from([
            "pbtdl",
            "legal test image",
            "--config",
            config_path.to_str().expect("UTF-8 path"),
            "--auto",
            "--dry-run",
        ])
        .expect("parse CLI");

        run_with_policy(cli, NavigationPolicy::local_test_pages())
            .await
            .expect("local fixture CLI dry run");

        source_server.abort();
        candidate_server.abort();
    }
}
