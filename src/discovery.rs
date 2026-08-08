//! Rendered discovery-page extraction, proxy validation, and bounded health caching.

use crate::browser::{
    BrowserError, BrowserResult, BrowserSession, NavigationPolicy, RenderedDocument,
};
use crate::config::DiscoveryConfig;
use async_trait::async_trait;
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use url::Url;

const HARD_MAX_SOURCE_PAGES: usize = 8;
const HARD_MAX_CANDIDATES: usize = 64;
const HARD_MAX_ANCHORS: usize = 600;
const HARD_MAX_FORMS: usize = 32;
const HARD_MAX_INPUTS_PER_FORM: usize = 24;
const HARD_MAX_CACHE_ENTRIES: usize = 64;
const CACHE_SCHEMA_VERSION: u32 = 1;
const CACHE_FILE_NAME: &str = "proxy-health.json";

#[async_trait]
pub trait PageRenderer: Send {
    async fn render(&mut self, url: &Url) -> BrowserResult<RenderedDocument>;
}

#[async_trait]
impl PageRenderer for BrowserSession {
    async fn render(&mut self, url: &Url) -> BrowserResult<RenderedDocument> {
        self.navigate(url).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CandidateUrl(Url);

impl CandidateUrl {
    pub fn as_url(&self) -> &Url {
        &self.0
    }

    pub fn into_url(self) -> Url {
        self.0
    }
}

impl fmt::Display for CandidateUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchForm {
    pub form_index: usize,
    pub input: QueryInput,
    pub action: Url,
    pub method: FormMethod,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryInput {
    pub input_index: usize,
    pub name: Option<String>,
    pub id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormMethod {
    Get,
    Post,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCandidate {
    pub candidate: CandidateUrl,
    pub rendered_url: Url,
    pub search_form: SearchForm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureStage {
    DiscoverySource,
    CandidatePreflight,
    CandidateRender,
    CandidateValidation,
}

impl fmt::Display for FailureStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DiscoverySource => "discovery source",
            Self::CandidatePreflight => "candidate preflight",
            Self::CandidateRender => "candidate render",
            Self::CandidateValidation => "candidate validation",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureReason {
    RejectedNavigation,
    Timeout,
    BrowserFailure,
    Blocked,
    Challenge,
    UnsupportedPage,
    MalformedPage,
}

impl fmt::Display for FailureReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RejectedNavigation => "navigation was rejected",
            Self::Timeout => "operation timed out",
            Self::BrowserFailure => "browser operation failed",
            Self::Blocked => "access was denied",
            Self::Challenge => "page presented a challenge or CAPTCHA",
            Self::UnsupportedPage => "page had no supported search form",
            Self::MalformedPage => "page structure was malformed",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryFailure {
    pub stage: FailureStage,
    pub url: String,
    pub reason: FailureReason,
}

impl fmt::Display for DiscoveryFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}: {}", self.stage, self.url, self.reason)
    }
}

#[derive(Debug)]
pub struct DiscoveryOutcome {
    pub candidates: Vec<ValidatedCandidate>,
    pub failures: Vec<DiscoveryFailure>,
    pub cache_warning: Option<String>,
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("no usable Pirate Bay-compatible proxy candidate was found")]
    NoUsableCandidates {
        failures: Vec<DiscoveryFailure>,
        cache_warning: Option<String>,
    },
}

impl DiscoveryError {
    pub fn failures(&self) -> &[DiscoveryFailure] {
        match self {
            Self::NoUsableCandidates { failures, .. } => failures,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveryEngine {
    policy: NavigationPolicy,
    cache_file: PathBuf,
    now_seconds: u64,
}

impl DiscoveryEngine {
    pub fn production(cache_dir: &Path) -> Self {
        Self::with_policy(cache_dir, NavigationPolicy::production())
    }

    pub(crate) fn with_policy(cache_dir: &Path, policy: NavigationPolicy) -> Self {
        Self {
            policy,
            cache_file: cache_dir.join(CACHE_FILE_NAME),
            now_seconds: unix_time_seconds(),
        }
    }

    #[cfg(test)]
    fn for_test(cache_dir: &Path, policy: NavigationPolicy, now_seconds: u64) -> Self {
        Self {
            policy,
            cache_file: cache_dir.join(CACHE_FILE_NAME),
            now_seconds,
        }
    }

    pub async fn discover_and_validate(
        &self,
        renderer: &mut dyn PageRenderer,
        settings: &DiscoveryConfig,
    ) -> Result<DiscoveryOutcome, DiscoveryError> {
        let source_limit = settings.max_source_pages.get().min(HARD_MAX_SOURCE_PAGES);
        let candidate_limit = settings.max_candidates.get().min(HARD_MAX_CANDIDATES);
        let mut failures = Vec::new();
        let mut cache = HealthCache::load(&self.cache_file);
        let mut candidates = Vec::new();
        let mut seen_hosts = HashSet::new();

        for cached in cache.fresh_successes(
            self.now_seconds,
            settings.cache_ttl_seconds,
            candidate_limit,
        ) {
            push_candidate(&mut candidates, &mut seen_hosts, cached, candidate_limit);
        }
        for seed in &settings.seed_candidates {
            if let Ok(candidate) = normalize_candidate(seed.clone()) {
                push_candidate(&mut candidates, &mut seen_hosts, candidate, candidate_limit);
            } else {
                failures.push(failure(
                    FailureStage::CandidatePreflight,
                    seed,
                    FailureReason::RejectedNavigation,
                ));
            }
        }

        for source in settings.source_pages.iter().take(source_limit) {
            if candidates.len() >= candidate_limit {
                break;
            }
            if self
                .policy
                .validate(source, Duration::from_secs(5))
                .await
                .is_err()
            {
                failures.push(failure(
                    FailureStage::DiscoverySource,
                    source,
                    FailureReason::RejectedNavigation,
                ));
                continue;
            }
            let rendered = match renderer.render(source).await {
                Ok(rendered) => rendered,
                Err(error) => {
                    failures.push(failure(
                        FailureStage::DiscoverySource,
                        source,
                        reason_from_browser_error(&error),
                    ));
                    continue;
                }
            };
            if self
                .policy
                .validate(&rendered.url, Duration::from_secs(5))
                .await
                .is_err()
            {
                failures.push(failure(
                    FailureStage::DiscoverySource,
                    &rendered.url,
                    FailureReason::RejectedNavigation,
                ));
                continue;
            }
            let remaining = candidate_limit.saturating_sub(candidates.len());
            for candidate in extract_candidates(&rendered, remaining) {
                push_candidate(&mut candidates, &mut seen_hosts, candidate, candidate_limit);
            }
        }

        let mut validated = Vec::new();
        for candidate in candidates {
            let url = candidate.as_url();
            if self
                .policy
                .validate(url, Duration::from_secs(5))
                .await
                .is_err()
            {
                failures.push(failure(
                    FailureStage::CandidatePreflight,
                    url,
                    FailureReason::RejectedNavigation,
                ));
                cache.record_failure(url, self.now_seconds);
                continue;
            }
            let rendered = match renderer.render(url).await {
                Ok(rendered) => rendered,
                Err(error) => {
                    failures.push(failure(
                        FailureStage::CandidateRender,
                        url,
                        reason_from_browser_error(&error),
                    ));
                    cache.record_failure(url, self.now_seconds);
                    continue;
                }
            };
            if self
                .policy
                .validate(&rendered.url, Duration::from_secs(5))
                .await
                .is_err()
            {
                failures.push(failure(
                    FailureStage::CandidateValidation,
                    &rendered.url,
                    FailureReason::RejectedNavigation,
                ));
                cache.record_failure(url, self.now_seconds);
                continue;
            }
            match validate_candidate_page(&rendered) {
                Ok(search_form) => {
                    cache.record_success(url, self.now_seconds);
                    validated.push(ValidatedCandidate {
                        candidate,
                        rendered_url: rendered.url,
                        search_form,
                    });
                }
                Err(reason) => {
                    failures.push(failure(FailureStage::CandidateValidation, url, reason));
                    cache.record_failure(url, self.now_seconds);
                }
            }
        }

        let cache_warning = cache
            .save(&self.cache_file)
            .err()
            .map(|_| "proxy health cache could not be updated".to_string());
        if validated.is_empty() {
            return Err(DiscoveryError::NoUsableCandidates {
                failures,
                cache_warning,
            });
        }
        Ok(DiscoveryOutcome {
            candidates: validated,
            failures,
            cache_warning,
        })
    }
}

pub fn extract_candidates(document: &RenderedDocument, limit: usize) -> Vec<CandidateUrl> {
    if limit == 0 {
        return Vec::new();
    }
    let html = Html::parse_document(&document.html);
    let host = document.url.host_str().unwrap_or_default();
    let selectors: &[(&str, bool)] = if host.eq_ignore_ascii_case("piratebayproxy.info") {
        &[
            ("table tr td:first-child a[href]", true),
            ("table a[href]", true),
        ]
    } else if host.eq_ignore_ascii_case("techpp.com") {
        &[("article a[href]", false), ("main a[href]", false)]
    } else {
        &[]
    };

    for (selector, trusted_list) in selectors {
        let extracted = extract_with_selector(&html, document, selector, *trusted_list, limit);
        if !extracted.is_empty() {
            return extracted;
        }
    }
    extract_with_selector(&html, document, "a[href]", false, limit)
}

pub fn normalize_candidate(mut url: Url) -> Result<CandidateUrl, FailureReason> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return Err(FailureReason::RejectedNavigation);
    }
    url.set_fragment(None);
    Ok(CandidateUrl(url))
}

pub fn validate_candidate_page(document: &RenderedDocument) -> Result<SearchForm, FailureReason> {
    if document.html.trim().is_empty() {
        return Err(FailureReason::MalformedPage);
    }
    let lower = document
        .html
        .chars()
        .take(400_000)
        .collect::<String>()
        .to_ascii_lowercase();
    if is_challenge_page(&lower) {
        return Err(FailureReason::Challenge);
    }
    if is_access_denied_page(&lower) {
        return Err(FailureReason::Blocked);
    }

    let html = Html::parse_document(&document.html);
    if !has_torrent_context(&html, &lower) {
        return Err(FailureReason::UnsupportedPage);
    }
    let form_selector = selector("form");
    let forms: Vec<_> = html.select(&form_selector).take(HARD_MAX_FORMS).collect();
    let known_selectors = [
        "form#searchform",
        "form[action*='search']",
        "form[action*='/s/']",
        "form.search",
    ];
    for known in known_selectors {
        let known_selector = selector(known);
        for matched in html.select(&known_selector) {
            if let Some((form_index, form)) = forms
                .iter()
                .enumerate()
                .find(|(_, form)| form.id() == matched.id())
            {
                if let Some(search_form) = parse_search_form(*form, form_index, &document.url, true)
                {
                    return Ok(search_form);
                }
            }
        }
    }
    forms
        .into_iter()
        .enumerate()
        .find_map(|(index, form)| parse_search_form(form, index, &document.url, false))
        .ok_or(FailureReason::UnsupportedPage)
}

fn extract_with_selector(
    html: &Html,
    document: &RenderedDocument,
    selector_text: &str,
    trusted_list: bool,
    limit: usize,
) -> Vec<CandidateUrl> {
    let selector = selector(selector_text);
    let mut output = Vec::new();
    let mut seen = HashSet::new();
    let source_key = host_key(&document.url);
    for anchor in html.select(&selector).take(HARD_MAX_ANCHORS) {
        if output.len() >= limit {
            break;
        }
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        let text = anchor.text().take(8).collect::<String>();
        if !trusted_list && !looks_like_candidate(&text, href) {
            continue;
        }
        let Ok(resolved) = document.url.join(href) else {
            continue;
        };
        let Ok(candidate) = normalize_candidate(resolved) else {
            continue;
        };
        let key = host_key(candidate.as_url());
        if key == source_key || !seen.insert(key) {
            continue;
        }
        output.push(candidate);
    }
    output
}

fn looks_like_candidate(text: &str, href: &str) -> bool {
    let value = format!("{text} {href}").to_ascii_lowercase();
    ["pirate", "tpb", "proxy", "mirrorbay", "hiddenbay"]
        .iter()
        .any(|needle| value.contains(needle))
}

fn parse_search_form(
    form: ElementRef<'_>,
    form_index: usize,
    page_url: &Url,
    known_shape: bool,
) -> Option<SearchForm> {
    let method = match form
        .value()
        .attr("method")
        .unwrap_or("get")
        .to_ascii_lowercase()
        .as_str()
    {
        "get" => FormMethod::Get,
        "post" => FormMethod::Post,
        _ => return None,
    };
    let action = page_url
        .join(form.value().attr("action").unwrap_or(""))
        .ok()?;
    if !matches!(action.scheme(), "http" | "https")
        || !action.username().is_empty()
        || action.password().is_some()
        || host_key(&action) != host_key(page_url)
    {
        return None;
    }

    let input_selector = selector("input");
    let mut best: Option<(usize, ElementRef<'_>, u8)> = None;
    for (input_index, input) in form
        .select(&input_selector)
        .take(HARD_MAX_INPUTS_PER_FORM)
        .enumerate()
    {
        let input_type = input
            .value()
            .attr("type")
            .unwrap_or("text")
            .to_ascii_lowercase();
        if matches!(
            input_type.as_str(),
            "hidden" | "password" | "submit" | "button" | "file" | "checkbox" | "radio"
        ) {
            continue;
        }
        let name = input.value().attr("name").unwrap_or_default();
        let id = input.value().attr("id").unwrap_or_default();
        let placeholder = input.value().attr("placeholder").unwrap_or_default();
        let mut score = 0;
        if name.eq_ignore_ascii_case("q") || name.eq_ignore_ascii_case("search") {
            score += 5;
        }
        if input_type == "search" {
            score += 4;
        }
        if id.to_ascii_lowercase().contains("search")
            || placeholder.to_ascii_lowercase().contains("search")
        {
            score += 2;
        }
        if score > best.as_ref().map_or(0, |(_, _, score)| *score) {
            best = Some((input_index, input, score));
        }
    }
    let (input_index, input, score) = best?;
    if score < if known_shape { 2 } else { 4 } {
        return None;
    }
    Some(SearchForm {
        form_index,
        input: QueryInput {
            input_index,
            name: input.value().attr("name").map(ToString::to_string),
            id: input.value().attr("id").map(ToString::to_string),
        },
        action,
        method,
    })
}

fn has_torrent_context(html: &Html, lower_html: &str) -> bool {
    let anchor_selector = selector("a[href]");
    let mut has_supporting_navigation = false;
    for anchor in html.select(&anchor_selector).take(HARD_MAX_ANCHORS) {
        let href = anchor.value().attr("href").unwrap_or_default();
        if href.to_ascii_lowercase().starts_with("magnet:") {
            return true;
        }
        let text = anchor.text().take(8).collect::<String>();
        let combined = format!("{href} {text}").to_ascii_lowercase();
        if ["browse", "recent", "top 100", "top/", "category", "torrent"]
            .iter()
            .any(|needle| combined.contains(needle))
        {
            has_supporting_navigation = true;
        }
    }
    lower_html.contains("torrent") && has_supporting_navigation
}

fn is_challenge_page(lower_html: &str) -> bool {
    [
        "cf-chl-",
        "g-recaptcha",
        "hcaptcha",
        "verify you are human",
        "checking your browser",
        "attention required! | cloudflare",
        "id=\"captcha",
        "class=\"captcha",
    ]
    .iter()
    .any(|marker| lower_html.contains(marker))
}

fn is_access_denied_page(lower_html: &str) -> bool {
    [
        "<title>access denied",
        ">access denied<",
        "request blocked",
        "you don't have permission to access",
        "you do not have permission to access",
    ]
    .iter()
    .any(|marker| lower_html.contains(marker))
}

fn push_candidate(
    output: &mut Vec<CandidateUrl>,
    seen_hosts: &mut HashSet<String>,
    candidate: CandidateUrl,
    limit: usize,
) {
    if output.len() >= limit {
        return;
    }
    if seen_hosts.insert(host_key(candidate.as_url())) {
        output.push(candidate);
    }
}

fn host_key(url: &Url) -> String {
    format!(
        "{}:{}",
        url.host_str().unwrap_or_default().to_ascii_lowercase(),
        url.port_or_known_default().unwrap_or_default()
    )
}

fn failure(stage: FailureStage, url: &Url, reason: FailureReason) -> DiscoveryFailure {
    DiscoveryFailure {
        stage,
        url: sanitized_url(url),
        reason,
    }
}

fn sanitized_url(url: &Url) -> String {
    let mut sanitized = url.clone();
    let _ = sanitized.set_username("");
    let _ = sanitized.set_password(None);
    sanitized.set_query(None);
    sanitized.set_fragment(None);
    sanitized.to_string()
}

fn reason_from_browser_error(error: &BrowserError) -> FailureReason {
    match error {
        BrowserError::NavigationRejected(_) | BrowserError::TooManyRedirects(_) => {
            FailureReason::RejectedNavigation
        }
        BrowserError::Timeout { .. } => FailureReason::Timeout,
        _ => FailureReason::BrowserFailure,
    }
}

fn selector(value: &str) -> Selector {
    Selector::parse(value).expect("static CSS selector must be valid")
}

fn unix_time_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HealthCache {
    schema_version: u32,
    entries: Vec<HealthEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HealthEntry {
    url: String,
    last_success: Option<u64>,
    last_failure: Option<u64>,
}

impl HealthCache {
    fn load(path: &Path) -> Self {
        let Ok(bytes) = fs::read(path) else {
            return Self::empty();
        };
        let Ok(mut cache) = serde_json::from_slice::<Self>(&bytes) else {
            return Self::empty();
        };
        if cache.schema_version != CACHE_SCHEMA_VERSION {
            return Self::empty();
        }
        cache.entries.truncate(HARD_MAX_CACHE_ENTRIES);
        cache
    }

    fn empty() -> Self {
        Self {
            schema_version: CACHE_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }

    fn fresh_successes(&self, now: u64, ttl: u64, limit: usize) -> Vec<CandidateUrl> {
        let mut entries: Vec<_> = self
            .entries
            .iter()
            .filter_map(|entry| {
                let success = entry.last_success?;
                let fresh = now.saturating_sub(success) <= ttl;
                let failure_is_newer = entry.last_failure.is_some_and(|failure| failure >= success);
                if !fresh || failure_is_newer {
                    return None;
                }
                let url = Url::parse(&entry.url).ok()?;
                normalize_candidate(url).ok().map(|url| (success, url))
            })
            .collect();
        entries.sort_by(|(left_time, left), (right_time, right)| {
            right_time
                .cmp(left_time)
                .then_with(|| left.to_string().cmp(&right.to_string()))
        });
        entries
            .into_iter()
            .take(limit)
            .map(|(_, url)| url)
            .collect()
    }

    fn record_success(&mut self, url: &Url, now: u64) {
        let entry = self.entry_mut(url);
        entry.last_success = Some(now);
        entry.last_failure = None;
    }

    fn record_failure(&mut self, url: &Url, now: u64) {
        let entry = self.entry_mut(url);
        entry.last_failure = Some(now);
    }

    fn entry_mut(&mut self, url: &Url) -> &mut HealthEntry {
        let mut cache_url = url.clone();
        cache_url.set_fragment(None);
        let normalized = cache_url.to_string();
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.url == normalized)
        {
            return &mut self.entries[index];
        }
        if self.entries.len() >= HARD_MAX_CACHE_ENTRIES {
            self.entries.remove(0);
        }
        self.entries.push(HealthEntry {
            url: normalized,
            last_success: None,
            last_failure: None,
        });
        self.entries.last_mut().expect("entry was just inserted")
    }

    fn save(&mut self, path: &Path) -> Result<(), CacheWriteError> {
        self.schema_version = CACHE_SCHEMA_VERSION;
        self.entries.truncate(HARD_MAX_CACHE_ENTRIES);
        let parent = path.parent().ok_or(CacheWriteError)?;
        fs::create_dir_all(parent).map_err(|_| CacheWriteError)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|_| CacheWriteError)?;
        serde_json::to_writer(&mut temporary, self).map_err(|_| CacheWriteError)?;
        temporary.write_all(b"\n").map_err(|_| CacheWriteError)?;
        temporary.flush().map_err(|_| CacheWriteError)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|_| CacheWriteError)?;
        temporary.persist(path).map_err(|_| CacheWriteError)?;
        Ok(())
    }
}

#[derive(Debug)]
struct CacheWriteError;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::BrowserSession;
    use crate::config::BrowserConfig;
    use std::collections::HashMap;
    use std::io;
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    const NOW: u64 = 10_000;

    fn document(url: &str, html: &str) -> RenderedDocument {
        RenderedDocument {
            url: Url::parse(url).expect("fixture URL"),
            html: html.to_string(),
        }
    }

    fn valid_page() -> &'static str {
        include_str!("../tests/fixtures/discovery/valid_candidate.html")
    }

    #[test]
    fn known_selector_extracts_normalizes_and_deduplicates_hosts() {
        let rendered = document(
            "https://piratebayproxy.info/",
            include_str!("../tests/fixtures/discovery/known_source.html"),
        );

        let candidates = extract_candidates(&rendered, 10);

        let values: Vec<_> = candidates.iter().map(ToString::to_string).collect();
        assert_eq!(
            values,
            [
                "https://thepiratebay.example/",
                "https://tpb.example/search",
            ]
        );
    }

    #[test]
    fn generic_fallback_is_bounded_and_ignores_unrelated_or_malformed_links() {
        let rendered = document(
            "https://93.184.216.1/list",
            include_str!("../tests/fixtures/discovery/generic_source.html"),
        );

        let candidates = extract_candidates(&rendered, 2);

        assert_eq!(candidates.len(), 2);
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.as_url().fragment().is_none())
        );
    }

    #[test]
    fn candidate_validation_distinguishes_blocked_challenged_deceptive_and_valid_pages() {
        let url = "https://93.184.216.2/";
        assert_eq!(
            validate_candidate_page(&document(
                url,
                include_str!("../tests/fixtures/discovery/blocked.html")
            )),
            Err(FailureReason::Blocked)
        );
        assert_eq!(
            validate_candidate_page(&document(
                url,
                include_str!("../tests/fixtures/discovery/challenge.html")
            )),
            Err(FailureReason::Challenge)
        );
        assert_eq!(
            validate_candidate_page(&document(
                url,
                include_str!("../tests/fixtures/discovery/deceptive.html")
            )),
            Err(FailureReason::UnsupportedPage)
        );

        let form = validate_candidate_page(&document(url, valid_page())).expect("valid form");
        assert_eq!(form.form_index, 0);
        assert_eq!(form.input.name.as_deref(), Some("q"));
        assert_eq!(form.action.as_str(), "https://93.184.216.2/search");
    }

    struct MockRenderer {
        pages: HashMap<String, Result<RenderedDocument, FailureReason>>,
        visited: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl PageRenderer for MockRenderer {
        async fn render(&mut self, url: &Url) -> BrowserResult<RenderedDocument> {
            self.visited
                .lock()
                .expect("visited lock")
                .push(url.to_string());
            match self.pages.get(url.as_str()) {
                Some(Ok(page)) => Ok(page.clone()),
                Some(Err(FailureReason::Timeout)) => {
                    Err(BrowserError::Timeout { stage: "fixture" })
                }
                _ => Err(BrowserError::Operation {
                    stage: "fixture",
                    message: "missing fixture".to_string(),
                }),
            }
        }
    }

    fn discovery_settings(source: Url, seeds: Vec<Url>) -> DiscoveryConfig {
        DiscoveryConfig {
            source_pages: vec![source],
            seed_candidates: seeds,
            max_source_pages: NonZeroUsize::new(4).expect("nonzero"),
            max_candidates: NonZeroUsize::new(16).expect("nonzero"),
            cache_ttl_seconds: 60,
        }
    }

    #[tokio::test]
    async fn validates_in_order_and_keeps_a_usable_fallback() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let source = Url::parse("https://93.184.216.1/list").expect("source URL");
        let first = Url::parse("https://93.184.216.2/").expect("first URL");
        let second = Url::parse("https://93.184.216.3/").expect("second URL");
        let source_html =
            format!("<a href=\"{first}\">TPB proxy one</a><a href=\"{second}\">TPB proxy two</a>");
        let visited = Arc::new(Mutex::new(Vec::new()));
        let mut renderer = MockRenderer {
            pages: HashMap::from([
                (
                    source.to_string(),
                    Ok(document(source.as_str(), &source_html)),
                ),
                (first.to_string(), Err(FailureReason::Timeout)),
                (
                    second.to_string(),
                    Ok(document(second.as_str(), valid_page())),
                ),
            ]),
            visited: Arc::clone(&visited),
        };
        let engine = DiscoveryEngine::for_test(temp.path(), NavigationPolicy::production(), NOW);

        let outcome = engine
            .discover_and_validate(&mut renderer, &discovery_settings(source, vec![]))
            .await
            .expect("one fallback should validate");

        assert_eq!(outcome.candidates.len(), 1);
        assert_eq!(outcome.candidates[0].candidate.as_url(), &second);
        assert_eq!(outcome.failures[0].reason, FailureReason::Timeout);
    }

    #[tokio::test]
    async fn rejects_private_seed_before_renderer_navigation() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let source = Url::parse("https://93.184.216.1/list").expect("source URL");
        let private = Url::parse("http://127.0.0.1/private").expect("private URL");
        let public = Url::parse("https://93.184.216.4/").expect("public URL");
        let visited = Arc::new(Mutex::new(Vec::new()));
        let mut renderer = MockRenderer {
            pages: HashMap::from([
                (
                    source.to_string(),
                    Ok(document(source.as_str(), "<html></html>")),
                ),
                (
                    public.to_string(),
                    Ok(document(public.as_str(), valid_page())),
                ),
            ]),
            visited: Arc::clone(&visited),
        };
        let engine = DiscoveryEngine::for_test(temp.path(), NavigationPolicy::production(), NOW);

        let outcome = engine
            .discover_and_validate(
                &mut renderer,
                &discovery_settings(source, vec![private.clone(), public]),
            )
            .await
            .expect("public candidate should validate");

        assert!(outcome.failures.iter().any(|failure| {
            failure.url == sanitized_url(&private)
                && failure.reason == FailureReason::RejectedNavigation
        }));
        assert!(
            !visited
                .lock()
                .expect("visited lock")
                .contains(&private.to_string())
        );
    }

    #[tokio::test]
    async fn rejects_a_candidate_redirect_to_local_network_and_reports_the_stage() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let source = Url::parse("https://93.184.216.1/list").expect("source URL");
        let candidate = Url::parse("https://93.184.216.5/").expect("candidate URL");
        let redirected = Url::parse("http://127.0.0.1/admin").expect("redirect URL");
        let visited = Arc::new(Mutex::new(Vec::new()));
        let mut renderer = MockRenderer {
            pages: HashMap::from([
                (
                    source.to_string(),
                    Ok(document(source.as_str(), "<html></html>")),
                ),
                (
                    candidate.to_string(),
                    Ok(RenderedDocument {
                        url: redirected.clone(),
                        html: valid_page().to_string(),
                    }),
                ),
            ]),
            visited,
        };
        let engine = DiscoveryEngine::for_test(temp.path(), NavigationPolicy::production(), NOW);

        let error = engine
            .discover_and_validate(&mut renderer, &discovery_settings(source, vec![candidate]))
            .await
            .expect_err("private redirect must reject the only candidate");

        assert!(error.failures().iter().any(|failure| {
            failure.stage == FailureStage::CandidateValidation
                && failure.url == sanitized_url(&redirected)
                && failure.reason == FailureReason::RejectedNavigation
        }));
    }

    #[tokio::test]
    async fn fresh_cached_success_is_revalidated_before_configured_fallback() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let source = Url::parse("https://93.184.216.1/list").expect("source URL");
        let cached = Url::parse("https://93.184.216.6/?entry=1").expect("cached URL");
        let fallback = Url::parse("https://93.184.216.7/").expect("fallback URL");
        let cache_path = temp.path().join(CACHE_FILE_NAME);
        let mut cache = HealthCache::empty();
        cache.record_success(&cached, NOW - 5);
        cache.save(&cache_path).expect("save cache");
        let visited = Arc::new(Mutex::new(Vec::new()));
        let mut renderer = MockRenderer {
            pages: HashMap::from([
                (
                    source.to_string(),
                    Ok(document(source.as_str(), "<html></html>")),
                ),
                (
                    cached.to_string(),
                    Ok(document(cached.as_str(), valid_page())),
                ),
                (
                    fallback.to_string(),
                    Ok(document(fallback.as_str(), valid_page())),
                ),
            ]),
            visited: Arc::clone(&visited),
        };
        let engine = DiscoveryEngine::for_test(temp.path(), NavigationPolicy::production(), NOW);

        let outcome = engine
            .discover_and_validate(
                &mut renderer,
                &discovery_settings(source.clone(), vec![fallback]),
            )
            .await
            .expect("cached and fallback candidates validate");

        assert_eq!(outcome.candidates[0].candidate.as_url(), &cached);
        assert_eq!(
            visited.lock().expect("visited lock").as_slice(),
            [
                source.to_string(),
                cached.to_string(),
                "https://93.184.216.7/".to_string()
            ]
        );
    }

    #[test]
    fn cache_orders_fresh_successes_and_discards_expired_or_corrupt_data() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let path = temp.path().join(CACHE_FILE_NAME);
        let recent = Url::parse("https://93.184.216.10/").expect("recent URL");
        let older = Url::parse("https://93.184.216.11/").expect("older URL");
        let expired = Url::parse("https://93.184.216.12/").expect("expired URL");
        let mut cache = HealthCache::empty();
        cache.record_success(&older, NOW - 20);
        cache.record_success(&recent, NOW - 5);
        cache.record_success(&expired, NOW - 500);
        cache.save(&path).expect("write cache");

        let loaded = HealthCache::load(&path);
        let values: Vec<_> = loaded
            .fresh_successes(NOW, 60, 10)
            .into_iter()
            .map(|value| value.to_string())
            .collect();
        assert_eq!(values, [recent.to_string(), older.to_string()]);

        fs::write(&path, b"not json").expect("corrupt cache");
        assert!(HealthCache::load(&path).entries.is_empty());
    }

    async fn serve(body: String) -> (Url, JoinHandle<io::Result<()>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind local server");
        let address = listener.local_addr().expect("server address");
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await?;
                let body = body.clone();
                tokio::spawn(async move {
                    let mut request = [0_u8; 4096];
                    let _ = stream.read(&mut request).await;
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
            Url::parse(&format!("http://{address}/")).expect("local URL"),
            task,
        )
    }

    #[tokio::test]
    #[ignore = "requires a system Chrome/Chromium executable"]
    async fn local_browser_falls_back_from_blocked_to_valid_candidate() {
        if crate::browser::locate_browser(None).is_err() {
            eprintln!("skipping: no system Chrome or Chromium executable");
            return;
        }
        let (blocked_url, blocked_server) =
            serve(include_str!("../tests/fixtures/discovery/blocked.html").to_string()).await;
        let (valid_url, valid_server) = serve(valid_page().to_string()).await;
        let source_body = format!(
            "<a href=\"{blocked_url}\">TPB proxy blocked</a><a href=\"{valid_url}\">TPB proxy valid</a>"
        );
        let (source_url, source_server) = serve(source_body).await;
        let temp = tempfile::tempdir().expect("temporary directory");
        let policy = NavigationPolicy::local_test_pages();
        let engine = DiscoveryEngine::for_test(temp.path(), policy, NOW);
        let settings = discovery_settings(source_url, vec![]);
        let browser_settings = BrowserConfig {
            navigation_timeout_seconds: 10,
            overall_timeout_seconds: 30,
            ..BrowserConfig::default()
        };
        let mut browser = BrowserSession::launch(&browser_settings, policy)
            .await
            .expect("launch browser");

        let outcome = engine
            .discover_and_validate(&mut browser, &settings)
            .await
            .expect("valid fallback");

        assert_eq!(outcome.candidates.len(), 1);
        assert_eq!(outcome.candidates[0].candidate.as_url(), &valid_url);
        assert!(
            outcome
                .failures
                .iter()
                .any(|failure| failure.reason == FailureReason::Blocked)
        );
        browser.shutdown().await.expect("shutdown browser");
        source_server.abort();
        blocked_server.abort();
        valid_server.abort();
    }
}
