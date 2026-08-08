//! Isolated Chromium ownership and bounded rendered-page navigation.

use crate::config::BrowserConfig as AppBrowserConfig;
use chromiumoxide::cdp::browser_protocol::browser::{
    SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
};
use chromiumoxide::cdp::browser_protocol::target::TargetId;
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures::StreamExt;
use std::collections::HashSet;
use std::ffi::OsString;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};
use url::{Host, Url};

const COMMON_BROWSER_PATHS: &[&str] = &[
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/snap/bin/chromium",
];
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum BrowserError {
    #[error("configured browser executable is unavailable: {0}")]
    InvalidExecutable(PathBuf),
    #[error("no Chrome or Chromium executable was found; configure browser.executable")]
    MissingExecutable,
    #[error("browser launch failed: {0}")]
    Launch(String),
    #[error("browser exited before the workflow completed")]
    PrematureExit,
    #[error("browser {stage} timed out")]
    Timeout { stage: &'static str },
    #[error("browser operation failed during {stage}: {message}")]
    Operation {
        stage: &'static str,
        message: String,
    },
    #[error("navigation rejected: {0}")]
    NavigationRejected(String),
    #[error("page opened an unexpected top-level window")]
    UnexpectedWindow,
    #[error("browser has no active page")]
    NoActivePage,
}

pub type BrowserResult<T> = Result<T, BrowserError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavigationPolicy {
    allow_local_test_pages: bool,
}

impl NavigationPolicy {
    pub fn production() -> Self {
        Self {
            allow_local_test_pages: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn local_test_pages() -> Self {
        Self {
            allow_local_test_pages: true,
        }
    }

    pub async fn validate(&self, url: &Url, resolution_timeout: Duration) -> BrowserResult<()> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(BrowserError::NavigationRejected(
                "only HTTP and HTTPS are permitted".to_string(),
            ));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(BrowserError::NavigationRejected(
                "URL credentials are not permitted".to_string(),
            ));
        }
        let host = url.host().ok_or_else(|| {
            BrowserError::NavigationRejected("URL must contain a host".to_string())
        })?;

        if self.allow_local_test_pages {
            return Ok(());
        }

        match host {
            Host::Ipv4(address) => reject_non_public_ip(IpAddr::V4(address))?,
            Host::Ipv6(address) => reject_non_public_ip(IpAddr::V6(address))?,
            Host::Domain(domain) => {
                let normalized = domain.trim_end_matches('.').to_ascii_lowercase();
                if normalized == "localhost" || normalized.ends_with(".localhost") {
                    return Err(BrowserError::NavigationRejected(
                        "loopback destinations are not permitted".to_string(),
                    ));
                }
                let port = url.port_or_known_default().ok_or_else(|| {
                    BrowserError::NavigationRejected("URL has no usable port".to_string())
                })?;
                let addresses = timeout(
                    resolution_timeout,
                    tokio::net::lookup_host((normalized.as_str(), port)),
                )
                .await
                .map_err(|_| BrowserError::Timeout {
                    stage: "host resolution",
                })?
                .map_err(|_| {
                    BrowserError::NavigationRejected(
                        "destination host could not be resolved".to_string(),
                    )
                })?;
                let mut found = false;
                for address in addresses {
                    found = true;
                    reject_non_public_ip(address.ip())?;
                }
                if !found {
                    return Err(BrowserError::NavigationRejected(
                        "destination host resolved to no addresses".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedDocument {
    pub url: Url,
    pub html: String,
}

#[derive(Debug)]
pub struct BrowserSession {
    browser: Option<Browser>,
    handler_task: Option<JoinHandle<Result<(), String>>>,
    profile: Option<TempDir>,
    page: Option<Page>,
    initial_targets: HashSet<TargetId>,
    navigation_timeout: Duration,
    deadline: Instant,
    policy: NavigationPolicy,
}

impl BrowserSession {
    pub async fn launch(
        settings: &AppBrowserConfig,
        policy: NavigationPolicy,
    ) -> BrowserResult<Self> {
        let executable = locate_browser(settings.executable.as_deref())?;
        let profile = tempfile::Builder::new()
            .prefix("pbtdl-chromium-")
            .tempdir()
            .map_err(|error| BrowserError::Launch(error.to_string()))?;
        let navigation_timeout = Duration::from_secs(settings.navigation_timeout_seconds);
        let overall_timeout = Duration::from_secs(settings.overall_timeout_seconds);
        let deadline = Instant::now() + overall_timeout;
        let plan = BrowserLaunchPlan {
            executable,
            profile_path: profile.path().to_path_buf(),
            headless: settings.headless,
            launch_timeout: navigation_timeout.min(overall_timeout),
        };
        let chromium_config = plan.build()?;
        let (mut browser, mut handler) = timeout(overall_timeout, Browser::launch(chromium_config))
            .await
            .map_err(|_| BrowserError::Timeout { stage: "launch" })?
            .map_err(|error| BrowserError::Launch(error.to_string()))?;

        let handler_task = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                event.map_err(|error| error.to_string())?;
            }
            Err("browser event stream ended".to_string())
        });

        if let Err(error) = browser
            .execute(SetDownloadBehaviorParams::new(
                SetDownloadBehaviorBehavior::Deny,
            ))
            .await
        {
            let _ = browser.kill().await;
            handler_task.abort();
            return Err(BrowserError::Operation {
                stage: "download policy",
                message: error.to_string(),
            });
        }

        let mut initial_targets = HashSet::new();
        if browser.fetch_targets().await.is_ok() {
            if let Ok(pages) = browser.pages().await {
                for page in pages {
                    initial_targets.insert(page.target_id().clone());
                    let _ = page.close().await;
                }
            }
        }

        Ok(Self {
            browser: Some(browser),
            handler_task: Some(handler_task),
            profile: Some(profile),
            page: None,
            initial_targets,
            navigation_timeout,
            deadline,
            policy,
        })
    }

    pub async fn navigate(&mut self, url: &Url) -> BrowserResult<RenderedDocument> {
        self.ensure_handler_running().await?;
        let operation_timeout = self.operation_timeout("navigation")?;
        self.policy.validate(url, operation_timeout).await?;
        self.close_active_page().await;

        let browser = self.browser.as_ref().ok_or(BrowserError::PrematureExit)?;
        let page = timed(
            operation_timeout,
            "page creation",
            browser.new_page("about:blank"),
        )
        .await
        .map_err(|error| map_cdp_error("page creation", error))?;
        if let Err(error) = timed(operation_timeout, "navigation", page.goto(url.as_str())).await {
            let _ = page.close().await;
            return Err(map_cdp_error("navigation", error));
        }

        self.page = Some(page);
        if let Err(error) = self.reject_unexpected_pages().await {
            self.close_active_page().await;
            return Err(error);
        }

        let final_url = self.current_url().await?;
        self.policy.validate(&final_url, operation_timeout).await?;
        let html = self.current_content().await?;
        self.ensure_handler_running().await?;
        Ok(RenderedDocument {
            url: final_url,
            html,
        })
    }

    pub async fn current_document(&mut self) -> BrowserResult<RenderedDocument> {
        let url = self.current_url().await?;
        let operation_timeout = self.operation_timeout("document read")?;
        self.policy.validate(&url, operation_timeout).await?;
        let html = self.current_content().await?;
        self.reject_unexpected_pages().await?;
        Ok(RenderedDocument { url, html })
    }

    pub fn profile_path(&self) -> Option<&Path> {
        self.profile.as_ref().map(TempDir::path)
    }

    pub async fn shutdown(&mut self) -> BrowserResult<()> {
        self.close_active_page().await;
        let mut first_error = None;
        if let Some(mut browser) = self.browser.take() {
            match timeout(CLEANUP_TIMEOUT, browser.close()).await {
                Ok(Ok(_)) => match timeout(CLEANUP_TIMEOUT, browser.wait()).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        first_error = Some(BrowserError::Operation {
                            stage: "shutdown",
                            message: error.to_string(),
                        });
                    }
                    Err(_) => {
                        first_error = Some(BrowserError::Timeout { stage: "shutdown" });
                        let _ = browser.kill().await;
                    }
                },
                Ok(Err(error)) => {
                    first_error = Some(BrowserError::Operation {
                        stage: "shutdown",
                        message: error.to_string(),
                    });
                    let _ = browser.kill().await;
                }
                Err(_) => {
                    first_error = Some(BrowserError::Timeout { stage: "shutdown" });
                    let _ = browser.kill().await;
                }
            }
        }
        if let Some(handler) = self.handler_task.take() {
            handler.abort();
            let _ = handler.await;
        }
        self.profile.take();
        first_error.map_or(Ok(()), Err)
    }

    async fn current_url(&self) -> BrowserResult<Url> {
        let page = self.page.as_ref().ok_or(BrowserError::NoActivePage)?;
        let duration = self.operation_timeout("URL read")?;
        let value = timed(duration, "URL read", page.url())
            .await
            .map_err(|error| map_cdp_error("URL read", error))?
            .ok_or_else(|| BrowserError::Operation {
                stage: "URL read",
                message: "page did not report a URL".to_string(),
            })?;
        Url::parse(&value).map_err(|_| {
            BrowserError::NavigationRejected("browser returned a malformed URL".to_string())
        })
    }

    async fn current_content(&self) -> BrowserResult<String> {
        let page = self.page.as_ref().ok_or(BrowserError::NoActivePage)?;
        let duration = self.operation_timeout("document read")?;
        timed(duration, "document read", page.content())
            .await
            .map_err(|error| map_cdp_error("document read", error))
    }

    async fn reject_unexpected_pages(&mut self) -> BrowserResult<()> {
        let active_id = self
            .page
            .as_ref()
            .ok_or(BrowserError::NoActivePage)?
            .target_id()
            .clone();
        let duration = self.operation_timeout("window check")?;
        let pages = timed(
            duration,
            "window check",
            self.browser
                .as_ref()
                .ok_or(BrowserError::PrematureExit)?
                .pages(),
        )
        .await
        .map_err(|error| map_cdp_error("window check", error))?;
        let mut unexpected = false;
        for page in pages {
            if page.target_id() != &active_id {
                let is_browser_created_blank = self.initial_targets.contains(page.target_id())
                    || (page.opener_id().is_none()
                        && matches!(
                            page.url().await.ok().flatten().as_deref(),
                            Some("about:blank") | Some("chrome://newtab/")
                        ));
                unexpected |= !is_browser_created_blank;
                let _ = page.close().await;
            }
        }
        if unexpected {
            Err(BrowserError::UnexpectedWindow)
        } else {
            Ok(())
        }
    }

    async fn close_active_page(&mut self) {
        if let Some(page) = self.page.take() {
            let _ = timeout(CLEANUP_TIMEOUT, page.close()).await;
        }
    }

    async fn ensure_handler_running(&mut self) -> BrowserResult<()> {
        let finished = self
            .handler_task
            .as_ref()
            .is_none_or(JoinHandle::is_finished);
        if !finished {
            return Ok(());
        }
        if let Some(handler) = self.handler_task.take() {
            let _ = handler.await;
        }
        Err(BrowserError::PrematureExit)
    }

    fn operation_timeout(&self, stage: &'static str) -> BrowserResult<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(BrowserError::Timeout { stage });
        }
        Ok(remaining.min(self.navigation_timeout))
    }
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        let Some(mut browser) = self.browser.take() else {
            return;
        };
        if let Some(handler) = self.handler_task.take() {
            handler.abort();
        }
        let profile = self.profile.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = browser.kill().await;
                drop(profile);
            });
        } else {
            drop(browser);
            drop(profile);
        }
    }
}

#[derive(Debug)]
struct BrowserLaunchPlan {
    executable: PathBuf,
    profile_path: PathBuf,
    headless: bool,
    launch_timeout: Duration,
}

impl BrowserLaunchPlan {
    fn build(&self) -> BrowserResult<BrowserConfig> {
        let builder = BrowserConfig::builder()
            .chrome_executable(&self.executable)
            .user_data_dir(&self.profile_path)
            .incognito()
            .respect_https_errors()
            .disable_cache()
            .launch_timeout(self.launch_timeout)
            .request_timeout(self.launch_timeout)
            .args([
                "--disable-component-update",
                "--disable-domain-reliability",
                "--disable-notifications",
                "--disable-save-password-bubble",
                "--no-default-browser-check",
            ]);
        let builder = if self.headless {
            builder.new_headless_mode()
        } else {
            builder.with_head()
        };
        builder.build().map_err(BrowserError::Launch)
    }
}

pub fn locate_browser(configured: Option<&Path>) -> BrowserResult<PathBuf> {
    let path_entries = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect::<Vec<PathBuf>>())
        .unwrap_or_default();
    locate_browser_with(configured, COMMON_BROWSER_PATHS, &path_entries)
}

fn locate_browser_with(
    configured: Option<&Path>,
    common_paths: &[&str],
    path_entries: &[PathBuf],
) -> BrowserResult<PathBuf> {
    if let Some(path) = configured {
        let resolved = if path.components().count() == 1 {
            find_on_path(path.as_os_str(), path_entries).unwrap_or_else(|| path.to_path_buf())
        } else {
            path.to_path_buf()
        };
        return is_executable(&resolved)
            .then_some(resolved)
            .ok_or_else(|| BrowserError::InvalidExecutable(path.to_path_buf()));
    }

    common_paths
        .iter()
        .map(PathBuf::from)
        .find(|path| is_executable(path))
        .or_else(|| {
            [
                OsString::from("google-chrome"),
                OsString::from("google-chrome-stable"),
                OsString::from("chromium"),
                OsString::from("chromium-browser"),
            ]
            .iter()
            .find_map(|name| find_on_path(name, path_entries))
        })
        .ok_or(BrowserError::MissingExecutable)
}

fn find_on_path(name: &std::ffi::OsStr, entries: &[PathBuf]) -> Option<PathBuf> {
    entries
        .iter()
        .map(|entry| entry.join(name))
        .find(|path| is_executable(path))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn reject_non_public_ip(address: IpAddr) -> BrowserResult<()> {
    let disallowed = match address {
        IpAddr::V4(address) => is_disallowed_ipv4(address),
        IpAddr::V6(address) => is_disallowed_ipv6(address),
    };
    if disallowed {
        Err(BrowserError::NavigationRejected(
            "local, private, and reserved network destinations are not permitted".to_string(),
        ))
    } else {
        Ok(())
    }
}

fn is_disallowed_ipv4(address: Ipv4Addr) -> bool {
    address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
        || address.is_multicast()
        || address.octets()[0] == 0
}

fn is_disallowed_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_disallowed_ipv4(mapped);
    }
    address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || (address.segments()[0] & 0xfe00) == 0xfc00
        || (address.segments()[0] & 0xffc0) == 0xfe80
        || (address.segments()[0] == 0x2001 && address.segments()[1] == 0x0db8)
}

async fn timed<T, E>(
    duration: Duration,
    stage: &'static str,
    future: impl Future<Output = Result<T, E>>,
) -> Result<T, TimedError<E>> {
    timeout(duration, future)
        .await
        .map_err(|_| TimedError::Timeout(stage))?
        .map_err(TimedError::Inner)
}

enum TimedError<E> {
    Timeout(&'static str),
    Inner(E),
}

fn map_cdp_error(
    stage: &'static str,
    error: TimedError<chromiumoxide::error::CdpError>,
) -> BrowserError {
    match error {
        TimedError::Timeout(stage) => BrowserError::Timeout { stage },
        TimedError::Inner(error) => BrowserError::Operation {
            stage,
            message: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn make_executable(path: &Path) {
        fs::write(path, b"#!/bin/sh\nexit 0\n").expect("write fake executable");
        let mut permissions = fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("make executable");
    }

    #[test]
    fn configured_executable_takes_precedence() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let configured = temp.path().join("configured-chrome");
        let common = temp.path().join("common-chromium");
        make_executable(&configured);
        make_executable(&common);
        let common_text = common.to_string_lossy().into_owned();

        let selected = locate_browser_with(Some(&configured), &[&common_text], &[])
            .expect("select configured executable");

        assert_eq!(selected, configured);
    }

    #[test]
    fn common_location_precedes_path_fallback() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let common = temp.path().join("chromium-common");
        let path_dir = temp.path().join("bin");
        fs::create_dir(&path_dir).expect("create PATH directory");
        make_executable(&common);
        make_executable(&path_dir.join("chromium"));
        let common_text = common.to_string_lossy().into_owned();

        let selected = locate_browser_with(None, &[&common_text], &[path_dir])
            .expect("select common executable");

        assert_eq!(selected, common);
    }

    #[test]
    fn missing_and_non_executable_paths_are_concise_errors() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let file = temp.path().join("not-executable");
        fs::write(&file, b"not executable").expect("write file");

        assert!(matches!(
            locate_browser_with(Some(&file), &[], &[]),
            Err(BrowserError::InvalidExecutable(_))
        ));
        assert!(matches!(
            locate_browser_with(None, &[], &[]),
            Err(BrowserError::MissingExecutable)
        ));
    }

    #[test]
    fn launch_plan_uses_owned_profile_and_safe_defaults() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let executable = temp.path().join("chromium");
        make_executable(&executable);
        let profile = tempfile::Builder::new()
            .prefix("pbtdl-chromium-")
            .tempdir_in(temp.path())
            .expect("create profile");
        let profile_path = profile.path().to_path_buf();
        let plan = BrowserLaunchPlan {
            executable,
            profile_path: profile_path.clone(),
            headless: true,
            launch_timeout: Duration::from_secs(3),
        };

        plan.build().expect("build Chromium configuration");
        assert!(profile_path.exists());
        drop(profile);
        assert!(!profile_path.exists());
    }

    #[tokio::test]
    async fn production_policy_rejects_local_and_unsupported_destinations() {
        let policy = NavigationPolicy::production();
        for value in [
            "file:///tmp/page.html",
            "http://localhost/",
            "http://127.0.0.1/",
            "http://10.1.2.3/",
            "http://[::1]/",
            "http://[fc00::1]/",
        ] {
            let url = Url::parse(value).expect("fixture URL");
            assert!(
                policy.validate(&url, Duration::from_secs(1)).await.is_err(),
                "accepted {url}"
            );
        }
    }

    async fn serve_one_page() -> (Url, JoinHandle<io::Result<()>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind local server");
        let address = listener.local_addr().expect("server address");
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await?;
            let body = "<!doctype html><title>pbtdl smoke</title><h1>Rendered locally</h1>";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await?;
            stream.shutdown().await
        });
        (
            Url::parse(&format!("http://{address}/")).expect("local URL"),
            task,
        )
    }

    async fn browser_smoke(headless: bool) {
        if locate_browser(None).is_err() {
            eprintln!("skipping: no system Chrome or Chromium executable");
            return;
        }
        let (url, server) = serve_one_page().await;
        let settings = AppBrowserConfig {
            headless,
            navigation_timeout_seconds: 10,
            overall_timeout_seconds: 30,
            ..AppBrowserConfig::default()
        };
        let mut session = BrowserSession::launch(&settings, NavigationPolicy::local_test_pages())
            .await
            .expect("launch browser");
        let profile_path = session.profile_path().expect("owned profile").to_path_buf();

        let rendered = session.navigate(&url).await.expect("render local page");

        assert!(rendered.html.contains("Rendered locally"));
        assert_eq!(rendered.url, url);
        server.await.expect("server task").expect("server response");
        session.shutdown().await.expect("shutdown browser");
        assert!(!profile_path.exists());
    }

    #[tokio::test]
    #[ignore = "requires a system Chrome/Chromium executable"]
    async fn local_headless_browser_smoke() {
        browser_smoke(true).await;
    }

    #[tokio::test]
    #[ignore = "requires a display and a system Chrome/Chromium executable"]
    async fn local_headful_browser_smoke() {
        browser_smoke(false).await;
    }
}
