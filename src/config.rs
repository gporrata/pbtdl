//! Persistent, versioned configuration and XDG path resolution.

use crate::cli::Cli;
use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use serde::Deserialize;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use url::Url;

pub const CONFIG_SCHEMA_VERSION: u32 = 1;
const HARD_MAX_SOURCE_PAGES: usize = 8;
const HARD_MAX_CANDIDATES: usize = 64;
const HARD_MAX_RESULTS: usize = 500;
const HARD_MAX_TITLE_CHARACTERS: usize = 512;
const HARD_MAX_BROWSER_TIMEOUT_SECONDS: u64 = 300;
pub const DEFAULT_CONFIG_TEMPLATE: &str = r#"# pbtdl configuration
# Settings in this file override built-in defaults. Command-line options override this file.
schema_version = 1

[discovery]
# Human-facing HTML pages whose rendered links are inspected for proxy candidates.
source_pages = [
  "https://piratebayproxy.info/",
  "https://techpp.com/2023/02/17/the-pirate-bay-proxy-list/",
]
# Direct candidates are validated in Chromium exactly like discovered candidates.
seed_candidates = []
max_source_pages = 4
max_candidates = 24
cache_ttl_seconds = 21600

[browser]
# executable = "/usr/bin/chromium"
headless = true
navigation_timeout_seconds = 15
selector_timeout_seconds = 10
overall_timeout_seconds = 90

[search]
result_limit = 10
max_title_characters = 120

[downloader]
client = "auto"
output_directory = "."
# pbtdl supports only stop-after-completion behavior.
seed_after_completion = false
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub config_file: PathBuf,
    pub cache_dir: PathBuf,
}

impl ConfigPaths {
    pub fn resolve(override_path: Option<&Path>, env: &ConfigEnvironment) -> Result<Self> {
        let config_file = match override_path {
            Some(path) => make_absolute(path)?,
            None => xdg_base(
                env.xdg_config_home.as_deref(),
                env.home.as_deref(),
                ".config",
                "XDG_CONFIG_HOME",
            )?
            .join("pbtdl/pbtdl.toml"),
        };
        let cache_dir = xdg_base(
            env.xdg_cache_home.as_deref(),
            env.home.as_deref(),
            ".cache",
            "XDG_CACHE_HOME",
        )?
        .join("pbtdl");

        Ok(Self {
            config_file,
            cache_dir,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigEnvironment {
    xdg_config_home: Option<OsString>,
    xdg_cache_home: Option<OsString>,
    home: Option<OsString>,
}

impl ConfigEnvironment {
    pub fn current() -> Self {
        Self {
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME"),
            xdg_cache_home: std::env::var_os("XDG_CACHE_HOME"),
            home: std::env::var_os("HOME"),
        }
    }

    #[cfg(test)]
    fn new(
        xdg_config_home: Option<PathBuf>,
        xdg_cache_home: Option<PathBuf>,
        home: Option<PathBuf>,
    ) -> Self {
        Self {
            xdg_config_home: xdg_config_home.map(PathBuf::into_os_string),
            xdg_cache_home: xdg_cache_home.map(PathBuf::into_os_string),
            home: home.map(PathBuf::into_os_string),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppConfig {
    pub discovery: DiscoveryConfig,
    pub browser: BrowserConfig,
    pub search: SearchConfig,
    pub downloader: DownloaderConfig,
}

impl AppConfig {
    pub fn apply_cli(&mut self, cli: &Cli) -> Result<()> {
        if let Some(output) = &cli.output {
            self.downloader.output_directory = output.clone();
        }
        if let Some(limit) = cli.results {
            self.search.result_limit = limit;
        }
        if cli.headful {
            self.browser.headless = false;
        }
        if let Some(client) = cli.client {
            self.downloader.client = client;
        }
        self.validate()
    }

    fn apply_document(&mut self, document: ConfigDocument) -> Result<()> {
        if document.schema_version != CONFIG_SCHEMA_VERSION {
            bail!(
                "unsupported configuration schema version {}; expected {}",
                document.schema_version,
                CONFIG_SCHEMA_VERSION
            );
        }

        if let Some(discovery) = document.discovery {
            if let Some(source_pages) = discovery.source_pages {
                self.discovery.source_pages = source_pages;
            }
            if let Some(seed_candidates) = discovery.seed_candidates {
                self.discovery.seed_candidates = seed_candidates;
            }
            if let Some(value) = discovery.max_source_pages {
                self.discovery.max_source_pages = value;
            }
            if let Some(value) = discovery.max_candidates {
                self.discovery.max_candidates = value;
            }
            if let Some(value) = discovery.cache_ttl_seconds {
                self.discovery.cache_ttl_seconds = value;
            }
        }

        if let Some(browser) = document.browser {
            if let Some(value) = browser.executable {
                self.browser.executable = Some(value);
            }
            if let Some(value) = browser.headless {
                self.browser.headless = value;
            }
            if let Some(value) = browser.navigation_timeout_seconds {
                self.browser.navigation_timeout_seconds = value;
            }
            if let Some(value) = browser.selector_timeout_seconds {
                self.browser.selector_timeout_seconds = value;
            }
            if let Some(value) = browser.overall_timeout_seconds {
                self.browser.overall_timeout_seconds = value;
            }
        }

        if let Some(search) = document.search {
            if let Some(value) = search.result_limit {
                self.search.result_limit = value;
            }
            if let Some(value) = search.max_title_characters {
                self.search.max_title_characters = value;
            }
        }

        if let Some(downloader) = document.downloader {
            if let Some(value) = downloader.client {
                self.downloader.client = value;
            }
            if let Some(value) = downloader.output_directory {
                self.downloader.output_directory = value;
            }
            if let Some(value) = downloader.seed_after_completion {
                self.downloader.seed_after_completion = value;
            }
        }

        self.validate()
    }

    fn validate(&self) -> Result<()> {
        if self.discovery.source_pages.is_empty() && self.discovery.seed_candidates.is_empty() {
            bail!("configuration must contain at least one discovery page or seed candidate");
        }
        for url in self
            .discovery
            .source_pages
            .iter()
            .chain(&self.discovery.seed_candidates)
        {
            validate_entry_url(url)?;
        }
        if self.browser.navigation_timeout_seconds == 0
            || self.browser.selector_timeout_seconds == 0
            || self.browser.overall_timeout_seconds == 0
        {
            bail!("browser timeouts must be greater than zero");
        }
        if self.browser.navigation_timeout_seconds > self.browser.overall_timeout_seconds
            || self.browser.selector_timeout_seconds > self.browser.overall_timeout_seconds
        {
            bail!("browser navigation and selector timeouts cannot exceed the overall timeout");
        }
        if self.browser.overall_timeout_seconds > HARD_MAX_BROWSER_TIMEOUT_SECONDS {
            bail!(
                "browser overall timeout cannot exceed {HARD_MAX_BROWSER_TIMEOUT_SECONDS} seconds"
            );
        }
        if self.discovery.max_source_pages.get() > HARD_MAX_SOURCE_PAGES
            || self.discovery.max_candidates.get() > HARD_MAX_CANDIDATES
            || self.discovery.source_pages.len() > HARD_MAX_SOURCE_PAGES
            || self.discovery.seed_candidates.len() > HARD_MAX_CANDIDATES
        {
            bail!(
                "discovery limits cannot exceed {HARD_MAX_SOURCE_PAGES} source pages or {HARD_MAX_CANDIDATES} candidates"
            );
        }
        if self.search.result_limit.get() > HARD_MAX_RESULTS
            || self.search.max_title_characters.get() > HARD_MAX_TITLE_CHARACTERS
        {
            bail!(
                "search limits cannot exceed {HARD_MAX_RESULTS} results or {HARD_MAX_TITLE_CHARACTERS} title characters"
            );
        }
        if self.downloader.seed_after_completion {
            bail!(
                "seed_after_completion=true is unsupported; pbtdl stops after download completion"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryConfig {
    pub source_pages: Vec<Url>,
    pub seed_candidates: Vec<Url>,
    pub max_source_pages: NonZeroUsize,
    pub max_candidates: NonZeroUsize,
    pub cache_ttl_seconds: u64,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            source_pages: [
                "https://piratebayproxy.info/",
                "https://techpp.com/2023/02/17/the-pirate-bay-proxy-list/",
            ]
            .into_iter()
            .map(|value| Url::parse(value).expect("built-in discovery URL must be valid"))
            .collect(),
            seed_candidates: Vec::new(),
            max_source_pages: NonZeroUsize::new(4).expect("nonzero constant"),
            max_candidates: NonZeroUsize::new(24).expect("nonzero constant"),
            cache_ttl_seconds: 21_600,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserConfig {
    pub executable: Option<PathBuf>,
    pub headless: bool,
    pub navigation_timeout_seconds: u64,
    pub selector_timeout_seconds: u64,
    pub overall_timeout_seconds: u64,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            executable: None,
            headless: true,
            navigation_timeout_seconds: 15,
            selector_timeout_seconds: 10,
            overall_timeout_seconds: 90,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchConfig {
    pub result_limit: NonZeroUsize,
    pub max_title_characters: NonZeroUsize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            result_limit: NonZeroUsize::new(10).expect("nonzero constant"),
            max_title_characters: NonZeroUsize::new(120).expect("nonzero constant"),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DownloaderPreference {
    Auto,
    Aria2c,
    TransmissionCli,
    QbittorrentNox,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloaderConfig {
    pub client: DownloaderPreference,
    pub output_directory: PathBuf,
    pub seed_after_completion: bool,
}

impl Default for DownloaderConfig {
    fn default() -> Self {
        Self {
            client: DownloaderPreference::Auto,
            output_directory: PathBuf::from("."),
            seed_after_completion: false,
        }
    }
}

#[derive(Debug)]
pub struct LoadedConfig {
    pub config: AppConfig,
    pub paths: ConfigPaths,
    pub created: bool,
}

pub fn load_or_create(paths: ConfigPaths) -> Result<LoadedConfig> {
    let created = create_default_file(&paths.config_file)?;
    let bytes = fs::read(&paths.config_file)
        .with_context(|| format!("cannot read configuration {}", paths.config_file.display()))?;
    let text = std::str::from_utf8(&bytes).with_context(|| {
        format!(
            "configuration {} is not valid UTF-8",
            paths.config_file.display()
        )
    })?;
    let document: ConfigDocument = toml::from_str(text).map_err(|error: toml::de::Error| {
        let line = error.span().map(|span| {
            text.as_bytes()[..span.start.min(text.len())]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count()
                + 1
        });
        let location = line.map_or_else(String::new, |line| format!(" near line {line}"));
        anyhow!(
            "cannot parse configuration {}{location}: TOML syntax or value type is invalid; the file was left unchanged",
            paths.config_file.display()
        )
    })?;
    let mut config = AppConfig::default();
    config.apply_document(document).with_context(|| {
        format!(
            "invalid configuration {}; the file was left unchanged",
            paths.config_file.display()
        )
    })?;

    Ok(LoadedConfig {
        config,
        paths,
        created,
    })
}

fn create_default_file(path: &Path) -> Result<bool> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("configuration path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create configuration directory {}", parent.display()))?;

    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(DEFAULT_CONFIG_TEMPLATE.as_bytes())
                .with_context(|| {
                    format!("cannot write default configuration {}", path.display())
                })?;
            file.sync_all()
                .with_context(|| format!("cannot sync default configuration {}", path.display()))?;
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error)
            .with_context(|| format!("cannot create default configuration {}", path.display())),
    }
}

fn xdg_base(
    xdg_value: Option<&OsStr>,
    home: Option<&OsStr>,
    fallback_name: &str,
    variable_name: &str,
) -> Result<PathBuf> {
    if let Some(value) = xdg_value.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            bail!("{variable_name} must contain an absolute path");
        }
        return Ok(path);
    }
    let home = home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot resolve {variable_name}: HOME is not set"))?;
    if !home.is_absolute() {
        bail!("HOME must contain an absolute path");
    }
    Ok(home.join(fallback_name))
}

fn make_absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .context("cannot resolve relative configuration path")
        .map(|current| current.join(path))
}

fn validate_entry_url(url: &Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        bail!("discovery entry must use HTTP or HTTPS");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("discovery entry must not contain credentials");
    }
    if url.host_str().is_none() {
        bail!("discovery entry must contain a host");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigDocument {
    schema_version: u32,
    discovery: Option<DiscoveryDocument>,
    browser: Option<BrowserDocument>,
    search: Option<SearchDocument>,
    downloader: Option<DownloaderDocument>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscoveryDocument {
    source_pages: Option<Vec<Url>>,
    seed_candidates: Option<Vec<Url>>,
    max_source_pages: Option<NonZeroUsize>,
    max_candidates: Option<NonZeroUsize>,
    cache_ttl_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserDocument {
    executable: Option<PathBuf>,
    headless: Option<bool>,
    navigation_timeout_seconds: Option<u64>,
    selector_timeout_seconds: Option<u64>,
    overall_timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchDocument {
    result_limit: Option<NonZeroUsize>,
    max_title_characters: Option<NonZeroUsize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DownloaderDocument {
    client: Option<DownloaderPreference>,
    output_directory: Option<PathBuf>,
    seed_after_completion: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn isolated_paths(root: &Path) -> ConfigPaths {
        ConfigPaths {
            config_file: root.join("config/pbtdl/pbtdl.toml"),
            cache_dir: root.join("cache/pbtdl"),
        }
    }

    #[test]
    fn missing_configuration_creates_a_commented_usable_template() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let paths = isolated_paths(temp.path());

        let loaded = load_or_create(paths.clone()).expect("create and load defaults");

        assert!(loaded.created);
        assert_eq!(loaded.paths, paths);
        assert_eq!(loaded.config, AppConfig::default());
        let contents = fs::read_to_string(&loaded.paths.config_file).expect("read template");
        assert_eq!(contents, DEFAULT_CONFIG_TEMPLATE);
        assert!(contents.contains("# Human-facing HTML pages"));
    }

    #[test]
    fn existing_configuration_is_loaded_without_being_overwritten() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let paths = isolated_paths(temp.path());
        fs::create_dir_all(paths.config_file.parent().expect("parent")).expect("create parent");
        let original = br#"schema_version = 1
[search]
result_limit = 3
"#;
        fs::write(&paths.config_file, original).expect("write config");

        let loaded = load_or_create(paths.clone()).expect("load existing");

        assert!(!loaded.created);
        assert_eq!(loaded.config.search.result_limit.get(), 3);
        assert_eq!(fs::read(paths.config_file).expect("read config"), original);
    }

    #[test]
    fn malformed_configuration_reports_path_and_preserves_bytes() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let paths = isolated_paths(temp.path());
        fs::create_dir_all(paths.config_file.parent().expect("parent")).expect("create parent");
        let original = b"schema_version = [definitely not TOML";
        fs::write(&paths.config_file, original).expect("write malformed config");

        let error = load_or_create(paths.clone()).expect_err("malformed TOML must fail");

        let message = format!("{error:#}");
        assert!(message.contains(&paths.config_file.display().to_string()));
        assert!(message.contains("left unchanged"));
        assert_eq!(fs::read(paths.config_file).expect("read config"), original);
    }

    #[test]
    fn malformed_configuration_does_not_echo_secret_values() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let paths = isolated_paths(temp.path());
        fs::create_dir_all(paths.config_file.parent().expect("parent")).expect("create parent");
        let secret = "do-not-print-this-token";
        let original = format!("schema_version = 1\nsecret = \"{secret}\" trailing");
        fs::write(&paths.config_file, &original).expect("write malformed config");

        let error = load_or_create(paths).expect_err("malformed TOML must fail");

        assert!(!format!("{error:#}").contains(secret));
    }

    #[test]
    fn unsupported_schema_reports_actionable_error_and_preserves_file() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let paths = isolated_paths(temp.path());
        fs::create_dir_all(paths.config_file.parent().expect("parent")).expect("create parent");
        let original = b"schema_version = 99\n";
        fs::write(&paths.config_file, original).expect("write config");

        let error = load_or_create(paths.clone()).expect_err("new schema must fail");

        assert!(format!("{error:#}").contains("unsupported configuration schema version 99"));
        assert_eq!(fs::read(paths.config_file).expect("read config"), original);
    }

    #[test]
    fn rejects_configuration_and_cli_values_above_hard_caps() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let paths = isolated_paths(temp.path());
        fs::create_dir_all(paths.config_file.parent().expect("parent")).expect("create parent");
        fs::write(
            &paths.config_file,
            "schema_version = 1\n[discovery]\nmax_candidates = 65\n",
        )
        .expect("write oversized config");
        assert!(load_or_create(paths).is_err());

        let cli = Cli::try_parse_from(["pbtdl", "query", "--results", "501"]).expect("parse CLI");
        let mut config = AppConfig::default();
        assert!(config.apply_cli(&cli).is_err());
    }

    #[test]
    fn resolves_xdg_and_home_fallback_paths_without_process_environment_changes() {
        let root = tempfile::tempdir().expect("temporary directory");
        let xdg = ConfigEnvironment::new(
            Some(root.path().join("xdg-config")),
            Some(root.path().join("xdg-cache")),
            None,
        );
        let xdg_paths = ConfigPaths::resolve(None, &xdg).expect("resolve XDG paths");
        assert_eq!(
            xdg_paths.config_file,
            root.path().join("xdg-config/pbtdl/pbtdl.toml")
        );
        assert_eq!(xdg_paths.cache_dir, root.path().join("xdg-cache/pbtdl"));

        let fallback = ConfigEnvironment::new(None, None, Some(root.path().to_path_buf()));
        let fallback_paths = ConfigPaths::resolve(None, &fallback).expect("resolve HOME fallbacks");
        assert_eq!(
            fallback_paths.config_file,
            root.path().join(".config/pbtdl/pbtdl.toml")
        );
        assert_eq!(fallback_paths.cache_dir, root.path().join(".cache/pbtdl"));
    }

    #[test]
    fn explicit_config_path_and_cli_values_override_without_rewriting_file() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let environment = ConfigEnvironment::new(
            None,
            Some(temp.path().join("cache")),
            Some(temp.path().to_path_buf()),
        );
        let explicit = temp.path().join("custom/settings.toml");
        let paths = ConfigPaths::resolve(Some(&explicit), &environment).expect("resolve paths");
        let mut loaded = load_or_create(paths).expect("load defaults");
        let original = fs::read(&loaded.paths.config_file).expect("read original");
        let cli = Cli::try_parse_from([
            "pbtdl",
            "query",
            "--results",
            "7",
            "--output",
            "/tmp/elsewhere",
        ])
        .expect("parse CLI");

        loaded.config.apply_cli(&cli).expect("valid CLI overrides");

        assert_eq!(loaded.config.search.result_limit.get(), 7);
        assert_eq!(
            loaded.config.downloader.output_directory,
            PathBuf::from("/tmp/elsewhere")
        );
        assert_eq!(
            fs::read(&loaded.paths.config_file).expect("read after merge"),
            original
        );
    }

    #[test]
    fn rejects_relative_xdg_paths() {
        let environment = ConfigEnvironment::new(
            Some(PathBuf::from("relative")),
            Some(PathBuf::from("relative")),
            Some(PathBuf::from("/tmp/home")),
        );

        let error = ConfigPaths::resolve(None, &environment).expect_err("relative XDG must fail");

        assert!(error.to_string().contains("XDG_CONFIG_HOME"));
    }
}
