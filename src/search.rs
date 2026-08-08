//! Deterministic rendered-form search and normalized torrent result extraction.

use crate::browser::{
    BrowserError, BrowserFormTarget, BrowserResult, BrowserSession, RenderedDocument,
};
use crate::discovery::{CandidateUrl, SearchForm, ValidatedCandidate, validate_candidate_page};
use crate::model::{MagnetUri, TorrentResult, sanitize_display_text};
use async_trait::async_trait;
use scraper::{ElementRef, Html, Selector};
use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;
use thiserror::Error;
use tokio::time::Instant;
use url::Url;

const HARD_MAX_RESULT_ROWS: usize = 500;
const HARD_MAX_ROW_LINKS: usize = 80;
const HARD_MAX_TEXT_CHARACTERS: usize = 16_384;
const HARD_MAX_TITLE_CHARACTERS: usize = 512;
const HARD_MAX_CATEGORY_CHARACTERS: usize = 128;
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(75);

#[async_trait]
pub trait SearchRenderer: Send {
    async fn open(&mut self, url: &Url) -> BrowserResult<RenderedDocument>;
    async fn submit(&mut self, form: &SearchForm, query: &str) -> BrowserResult<()>;
    async fn current(&mut self) -> BrowserResult<RenderedDocument>;
}

#[async_trait]
impl SearchRenderer for BrowserSession {
    async fn open(&mut self, url: &Url) -> BrowserResult<RenderedDocument> {
        self.navigate(url).await
    }

    async fn submit(&mut self, form: &SearchForm, query: &str) -> BrowserResult<()> {
        self.submit_form(
            &BrowserFormTarget {
                form_index: form.form_index,
                input_index: form.input.input_index,
                input_name: form.input.name.clone(),
                input_id: form.input.id.clone(),
            },
            query,
        )
        .await
    }

    async fn current(&mut self) -> BrowserResult<RenderedDocument> {
        self.current_document().await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchFailureStage {
    CandidateOpen,
    FormValidation,
    FormSubmission,
    ResultWait,
    ResultParsing,
}

impl fmt::Display for SearchFailureStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CandidateOpen => "candidate open",
            Self::FormValidation => "form validation",
            Self::FormSubmission => "form submission",
            Self::ResultWait => "result wait",
            Self::ResultParsing => "result parsing",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchFailureReason {
    Timeout,
    BrowserFailure,
    UnsupportedPage,
    MalformedResults,
}

impl fmt::Display for SearchFailureReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Timeout => "operation timed out",
            Self::BrowserFailure => "browser operation failed",
            Self::UnsupportedPage => "search form was no longer supported",
            Self::MalformedResults => "rendered results contained no valid torrent identities",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchFailure {
    pub stage: SearchFailureStage,
    pub candidate: String,
    pub reason: SearchFailureReason,
}

impl fmt::Display for SearchFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {}: {}",
            self.stage, self.candidate, self.reason
        )
    }
}

#[derive(Debug)]
pub struct SearchOutcome {
    pub candidate: CandidateUrl,
    pub results: Vec<TorrentResult>,
    pub failures: Vec<SearchFailure>,
}

#[derive(Debug, Error)]
#[error("all validated proxy candidates failed during rendered search")]
pub struct SearchError {
    pub failures: Vec<SearchFailure>,
}

#[derive(Debug, Clone)]
pub struct SearchEngine {
    result_timeout: Duration,
    poll_interval: Duration,
}

impl SearchEngine {
    pub fn new(result_timeout: Duration) -> Self {
        Self {
            result_timeout,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }

    #[cfg(test)]
    fn for_test(result_timeout: Duration, poll_interval: Duration) -> Self {
        Self {
            result_timeout,
            poll_interval,
        }
    }

    pub async fn search_candidates(
        &self,
        renderer: &mut dyn SearchRenderer,
        candidates: &[ValidatedCandidate],
        query: &str,
    ) -> Result<SearchOutcome, SearchError> {
        let mut failures = Vec::new();
        for validated in candidates {
            let candidate_url = validated.candidate.as_url();
            let opened = match renderer.open(candidate_url).await {
                Ok(document) => document,
                Err(error) => {
                    failures.push(search_failure(
                        SearchFailureStage::CandidateOpen,
                        candidate_url,
                        browser_reason(&error),
                    ));
                    continue;
                }
            };
            let search_form = match validate_candidate_page(&opened) {
                Ok(form) => form,
                Err(_) => {
                    failures.push(search_failure(
                        SearchFailureStage::FormValidation,
                        candidate_url,
                        SearchFailureReason::UnsupportedPage,
                    ));
                    continue;
                }
            };
            if let Err(error) = renderer.submit(&search_form, query).await {
                failures.push(search_failure(
                    SearchFailureStage::FormSubmission,
                    candidate_url,
                    browser_reason(&error),
                ));
                continue;
            }

            let rendered = match self.wait_for_result_state(renderer).await {
                Ok(rendered) => rendered,
                Err(reason) => {
                    failures.push(search_failure(
                        SearchFailureStage::ResultWait,
                        candidate_url,
                        reason,
                    ));
                    continue;
                }
            };
            if rendered.state == ResultState::Empty {
                return Ok(SearchOutcome {
                    candidate: validated.candidate.clone(),
                    results: Vec::new(),
                    failures,
                });
            }
            let results = parse_rendered_results(&rendered.document);
            if results.is_empty() {
                failures.push(search_failure(
                    SearchFailureStage::ResultParsing,
                    candidate_url,
                    SearchFailureReason::MalformedResults,
                ));
                continue;
            }
            return Ok(SearchOutcome {
                candidate: validated.candidate.clone(),
                results,
                failures,
            });
        }
        Err(SearchError { failures })
    }

    async fn wait_for_result_state(
        &self,
        renderer: &mut dyn SearchRenderer,
    ) -> Result<RecognizedDocument, SearchFailureReason> {
        let deadline = Instant::now() + self.result_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SearchFailureReason::Timeout);
            }
            let document = match tokio::time::timeout(remaining, renderer.current()).await {
                Err(_) => return Err(SearchFailureReason::Timeout),
                Ok(Ok(document)) => document,
                Ok(Err(BrowserError::Operation {
                    stage: "URL read" | "document read",
                    ..
                })) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    tokio::time::sleep(self.poll_interval.min(remaining)).await;
                    continue;
                }
                Ok(Err(BrowserError::Timeout { .. })) => {
                    return Err(SearchFailureReason::Timeout);
                }
                Ok(Err(_)) => return Err(SearchFailureReason::BrowserFailure),
            };
            let state = recognize_result_state(&document);
            if state != ResultState::Pending {
                return Ok(RecognizedDocument { document, state });
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            tokio::time::sleep(self.poll_interval.min(remaining)).await;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultState {
    Pending,
    Rows,
    Empty,
}

struct RecognizedDocument {
    document: RenderedDocument,
    state: ResultState,
}

fn recognize_result_state(document: &RenderedDocument) -> ResultState {
    let lower = document
        .html
        .chars()
        .take(400_000)
        .collect::<String>()
        .to_ascii_lowercase();
    if [
        "no results found",
        "no hits. try adding an asterisk",
        "nothing found",
        "0 results",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return ResultState::Empty;
    }
    let html = Html::parse_document(&document.html);
    for selector_text in [
        "table#searchResult",
        "table#search-result",
        "ol#torrents",
        "ul.search-results",
        "[data-torrent-row]",
        "a[href^='magnet:']",
    ] {
        if html.select(&selector(selector_text)).next().is_some() {
            return ResultState::Rows;
        }
    }
    ResultState::Pending
}

pub fn parse_rendered_results(document: &RenderedDocument) -> Vec<TorrentResult> {
    let html = Html::parse_document(&document.html);
    for selector_text in [
        "table#searchResult tr",
        "table#search-result tr",
        "ol#torrents > li",
        "ul.search-results > li",
        "[data-torrent-row]",
    ] {
        let rows = parse_selected_rows(&html, document, selector_text);
        if !rows.is_empty() {
            return normalize_results(rows);
        }
    }
    normalize_results(parse_selected_rows(
        &html,
        document,
        "tr, li, article, div.result, div.torrent",
    ))
}

pub fn normalize_results(mut results: Vec<TorrentResult>) -> Vec<TorrentResult> {
    results.sort_by(|left, right| {
        right
            .seeders
            .unwrap_or(0)
            .cmp(&left.seeders.unwrap_or(0))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.info_hash().cmp(right.info_hash()))
            .then_with(|| left.source_host.cmp(&right.source_host))
    });
    let mut seen = HashSet::new();
    results.retain(|result| seen.insert(result.info_hash().clone()));
    results
}

fn parse_selected_rows(
    html: &Html,
    document: &RenderedDocument,
    selector_text: &str,
) -> Vec<TorrentResult> {
    let row_selector = selector(selector_text);
    html.select(&row_selector)
        .take(HARD_MAX_RESULT_ROWS)
        .filter_map(|row| parse_result_row(row, document))
        .collect()
}

fn parse_result_row(row: ElementRef<'_>, document: &RenderedDocument) -> Option<TorrentResult> {
    let anchor_selector = selector("a[href]");
    let mut magnet = None;
    for anchor in row.select(&anchor_selector).take(HARD_MAX_ROW_LINKS) {
        let href = anchor.value().attr("href")?;
        if let Ok(parsed) = MagnetUri::from_str(href) {
            magnet = Some(parsed);
            break;
        }
    }
    let magnet = magnet?;
    let name = extract_name(row, &magnet)?;
    let cells: Vec<String> = row
        .select(&selector("td"))
        .map(element_text)
        .take(32)
        .collect();
    let seeders = extract_count(
        row,
        &["data-seeders", "data-seeds"],
        &[".seeders", ".seeds", ".item-seed"],
    )
    .or_else(|| {
        (cells.len() >= 2)
            .then(|| parse_count(&cells[cells.len() - 2]))
            .flatten()
    });
    let leechers = extract_count(
        row,
        &["data-leechers", "data-leeches", "data-peers"],
        &[".leechers", ".leeches", ".item-leech", ".peers"],
    )
    .or_else(|| cells.last().and_then(|value| parse_count(value)));
    let size_bytes = extract_size(row);
    let category = extract_category(row);
    let source_host = document.url.host_str()?.to_ascii_lowercase();
    Some(TorrentResult {
        name,
        magnet,
        seeders,
        leechers,
        size_bytes,
        category,
        source_host,
    })
}

fn extract_name(row: ElementRef<'_>, magnet: &MagnetUri) -> Option<String> {
    for selector_text in [
        "a.detLink",
        ".detName a",
        ".item-name",
        ".name",
        "[data-title]",
        "[data-name]",
        "a[href*='/torrent/']",
        "a[href*='/description/']",
    ] {
        for element in row.select(&selector(selector_text)).take(4) {
            let attribute = element
                .value()
                .attr("data-title")
                .or_else(|| element.value().attr("data-name"));
            let value = attribute.map_or_else(|| element_text(element), ToString::to_string);
            let value = sanitize_display_text(&value, HARD_MAX_TITLE_CHARACTERS);
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    Url::parse(magnet.as_str())
        .ok()?
        .query_pairs()
        .find(|(key, _)| key == "dn")
        .map(|(_, value)| sanitize_display_text(&value, HARD_MAX_TITLE_CHARACTERS))
        .filter(|value| !value.is_empty())
}

fn extract_count(row: ElementRef<'_>, attributes: &[&str], selectors: &[&str]) -> Option<u64> {
    for attribute in attributes {
        if let Some(value) = row.value().attr(attribute).and_then(parse_count) {
            return Some(value);
        }
        let attribute_selector = selector(&format!("[{attribute}]"));
        if let Some(value) = row
            .select(&attribute_selector)
            .find_map(|element| element.value().attr(attribute).and_then(parse_count))
        {
            return Some(value);
        }
    }
    selectors.iter().find_map(|selector_text| {
        row.select(&selector(selector_text))
            .find_map(|element| parse_count(&element_text(element)))
    })
}

fn extract_size(row: ElementRef<'_>) -> Option<u64> {
    for attribute in ["data-size", "data-bytes"] {
        if let Some(value) = row.value().attr(attribute) {
            if let Some(size) = parse_size_value(value, true) {
                return Some(size);
            }
        }
        let attribute_selector = selector(&format!("[{attribute}]"));
        if let Some(size) = row.select(&attribute_selector).find_map(|element| {
            element
                .value()
                .attr(attribute)
                .and_then(|value| parse_size_value(value, true))
        }) {
            return Some(size);
        }
    }
    for selector_text in [".size", ".item-size", ".detDesc", ".description"] {
        if let Some(size) = row
            .select(&selector(selector_text))
            .find_map(|element| parse_size_value(&element_text(element), false))
        {
            return Some(size);
        }
    }
    parse_size_value(&element_text(row), false)
}

fn extract_category(row: ElementRef<'_>) -> Option<String> {
    for selector_text in [".category", ".item-type", ".type"] {
        if let Some(value) = row
            .select(&selector(selector_text))
            .map(element_text)
            .map(|value| sanitize_display_text(&value, HARD_MAX_CATEGORY_CHARACTERS))
            .find(|value| !value.is_empty())
        {
            return Some(value);
        }
    }
    row.select(&selector("a[href]"))
        .find(|anchor| {
            anchor.value().attr("href").is_some_and(|href| {
                let lower = href.to_ascii_lowercase();
                lower.contains("/browse/") || lower.contains("/category/")
            })
        })
        .map(element_text)
        .map(|value| sanitize_display_text(&value, HARD_MAX_CATEGORY_CHARACTERS))
        .filter(|value| !value.is_empty())
}

pub fn parse_count(value: &str) -> Option<u64> {
    let normalized: String = value
        .chars()
        .filter(|character| !matches!(character, ',' | '_' | ' ' | '\u{a0}'))
        .collect();
    (!normalized.is_empty()
        && normalized
            .chars()
            .all(|character| character.is_ascii_digit()))
    .then(|| normalized.parse().ok())
    .flatten()
}

pub fn parse_size(value: &str) -> Option<u64> {
    parse_size_value(value, true)
}

fn parse_size_value(value: &str, allow_plain_bytes: bool) -> Option<u64> {
    let cleaned = value.replace(',', "");
    let tokens: Vec<_> = cleaned.split_whitespace().collect();
    if allow_plain_bytes && tokens.len() == 1 {
        return parse_count(tokens[0]);
    }
    for pair in tokens.windows(2) {
        let number_text =
            pair[0].trim_matches(|character: char| !character.is_ascii_digit() && character != '.');
        let Ok(number) = number_text.parse::<f64>() else {
            continue;
        };
        let unit = pair[1]
            .trim_matches(|character: char| !character.is_ascii_alphabetic())
            .to_ascii_lowercase();
        let multiplier = match unit.as_str() {
            "b" | "byte" | "bytes" => 1_f64,
            "kb" | "kib" => 1024_f64,
            "mb" | "mib" => 1024_f64.powi(2),
            "gb" | "gib" => 1024_f64.powi(3),
            "tb" | "tib" => 1024_f64.powi(4),
            "pb" | "pib" => 1024_f64.powi(5),
            _ => continue,
        };
        let bytes = number * multiplier;
        if bytes.is_finite() && bytes >= 0.0 && bytes <= u64::MAX as f64 {
            return Some(bytes.round() as u64);
        }
    }
    None
}

fn element_text(element: ElementRef<'_>) -> String {
    element
        .text()
        .take(HARD_MAX_TEXT_CHARACTERS)
        .collect::<Vec<_>>()
        .join(" ")
}

fn search_failure(
    stage: SearchFailureStage,
    candidate: &Url,
    reason: SearchFailureReason,
) -> SearchFailure {
    let mut sanitized = candidate.clone();
    sanitized.set_query(None);
    sanitized.set_fragment(None);
    SearchFailure {
        stage,
        candidate: sanitized.to_string(),
        reason,
    }
}

fn browser_reason(error: &BrowserError) -> SearchFailureReason {
    if matches!(error, BrowserError::Timeout { .. }) {
        SearchFailureReason::Timeout
    } else {
        SearchFailureReason::BrowserFailure
    }
}

fn selector(value: &str) -> Selector {
    Selector::parse(value).expect("static or internally constructed CSS selector must be valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::NavigationPolicy;
    use crate::config::BrowserConfig;
    use crate::discovery::{FormMethod, QueryInput, normalize_candidate};
    use data_encoding::BASE32_NOPAD;
    use std::collections::HashMap;
    use std::io;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    const HASH_ONE: &str = "0123456789abcdef0123456789abcdef01234567";

    fn document(url: &str, html: &str) -> RenderedDocument {
        RenderedDocument {
            url: Url::parse(url).expect("fixture URL"),
            html: html.to_string(),
        }
    }

    #[test]
    fn parses_classic_modern_and_generic_rendered_shapes() {
        let classic = parse_rendered_results(&document(
            "https://proxy.example/search?q=test",
            include_str!("../tests/fixtures/search/classic.html"),
        ));
        assert_eq!(classic.len(), 1);
        assert_eq!(classic[0].name, "Ubuntu 🐧 résumé");
        assert_eq!(classic[0].seeders, Some(1_234));
        assert_eq!(classic[0].leechers, Some(56));
        assert_eq!(classic[0].size_bytes, Some(1_610_612_736));
        assert_eq!(classic[0].category.as_deref(), Some("Applications"));

        let modern = parse_rendered_results(&document(
            "https://proxy.example/search",
            include_str!("../tests/fixtures/search/modern.html"),
        ));
        assert_eq!(modern.len(), 1);
        assert_eq!(modern[0].seeders, Some(200));
        assert_eq!(modern[0].leechers, None);
        assert_eq!(modern[0].size_bytes, Some(734_003_200));

        let generic = parse_rendered_results(&document(
            "https://proxy.example/find",
            include_str!("../tests/fixtures/search/generic.html"),
        ));
        assert_eq!(generic.len(), 1);
        assert_eq!(generic[0].source_host, "proxy.example");
    }

    #[test]
    fn accepts_base32_hashes_and_deduplicates_canonical_identity() {
        let bytes: Vec<u8> = (0..HASH_ONE.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&HASH_ONE[index..index + 2], 16).expect("hex"))
            .collect();
        let base32 = BASE32_NOPAD.encode(&bytes);
        let html = format!(
            r#"<table id="searchResult">
              <tr data-seeders="10"><td><a class="detLink">hex</a><a href="magnet:?xt=urn:btih:{HASH_ONE}&dn=hex">magnet</a></td></tr>
              <tr data-seeders="50"><td><a class="detLink">base32</a><a href="magnet:?xt=urn:btih:{base32}&dn=base32">magnet</a></td></tr>
            </table>"#
        );

        let results = parse_rendered_results(&document("https://proxy.example/search", &html));

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "base32");
        assert_eq!(results[0].info_hash().as_str(), HASH_ONE);
    }

    #[test]
    fn rejects_invalid_and_misleading_magnet_like_links() {
        let results = parse_rendered_results(&document(
            "https://proxy.example/search",
            include_str!("../tests/fixtures/search/malformed.html"),
        ));

        assert!(results.is_empty());
    }

    #[test]
    fn hostile_titles_are_inert_and_bounded() {
        let oversized = format!("safe\u{1b}[31m\u{202e}{}", "x".repeat(2_000));
        let html = format!(
            r#"<table id="searchResult"><tr><td><a class="detLink">{oversized}</a><a href="magnet:?xt=urn:btih:{HASH_ONE}">magnet</a></td></tr></table>"#
        );

        let results = parse_rendered_results(&document("https://proxy.example/search", &html));

        assert_eq!(results.len(), 1);
        assert!(!results[0].name.contains('\u{1b}'));
        assert!(!results[0].name.contains('\u{202e}'));
        assert_eq!(results[0].name.chars().count(), HARD_MAX_TITLE_CHARACTERS);
    }

    #[test]
    fn parses_validated_counts_and_sizes_without_coercing_malformed_values() {
        assert_eq!(parse_count("1,234"), Some(1_234));
        assert_eq!(parse_count("unknown"), None);
        assert_eq!(parse_count("-1"), None);
        assert_eq!(parse_size("1.5 GiB"), Some(1_610_612_736));
        assert_eq!(parse_size("700 MB"), Some(734_003_200));
        assert_eq!(parse_size("4096"), Some(4_096));
        assert_eq!(parse_size("many GB"), None);
    }

    #[test]
    fn recognizes_zero_results_without_inventing_rows() {
        let page = document(
            "https://proxy.example/search",
            include_str!("../tests/fixtures/search/zero.html"),
        );
        assert_eq!(recognize_result_state(&page), ResultState::Empty);
        assert!(parse_rendered_results(&page).is_empty());
    }

    #[test]
    fn tie_ordering_is_deterministic() {
        let make = |name: &str, hash: &str| TorrentResult {
            name: name.to_string(),
            magnet: MagnetUri::from_str(&format!("magnet:?xt=urn:btih:{hash}&dn={name}"))
                .expect("magnet"),
            seeders: Some(10),
            leechers: None,
            size_bytes: None,
            category: None,
            source_host: "proxy.example".to_string(),
        };
        let results = normalize_results(vec![
            make("Zulu", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            make("alpha", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        ]);

        assert_eq!(results[0].name, "alpha");
        assert_eq!(results[1].name, "Zulu");
    }

    fn candidate(url: &str) -> ValidatedCandidate {
        let url = Url::parse(url).expect("candidate URL");
        ValidatedCandidate {
            candidate: normalize_candidate(url.clone()).expect("normalized candidate"),
            rendered_url: url.clone(),
            search_form: SearchForm {
                form_index: 0,
                input: QueryInput {
                    input_index: 0,
                    name: Some("q".to_string()),
                    id: None,
                },
                action: url.join("search").expect("search action"),
                method: FormMethod::Get,
            },
        }
    }

    struct TransitionRenderer {
        calls: usize,
        document: RenderedDocument,
    }

    #[async_trait]
    impl SearchRenderer for TransitionRenderer {
        async fn open(&mut self, _url: &Url) -> BrowserResult<RenderedDocument> {
            unreachable!("wait-state test does not open a page")
        }

        async fn submit(&mut self, _form: &SearchForm, _query: &str) -> BrowserResult<()> {
            unreachable!("wait-state test does not submit a form")
        }

        async fn current(&mut self) -> BrowserResult<RenderedDocument> {
            self.calls += 1;
            if self.calls == 1 {
                Err(BrowserError::Operation {
                    stage: "document read",
                    message: "navigation context changed".to_string(),
                })
            } else {
                Ok(self.document.clone())
            }
        }
    }

    #[tokio::test]
    async fn result_wait_retries_transient_document_context_changes() {
        let mut renderer = TransitionRenderer {
            calls: 0,
            document: document(
                "https://proxy.example/search",
                include_str!("../tests/fixtures/search/zero.html"),
            ),
        };
        let engine = SearchEngine::for_test(Duration::from_secs(1), Duration::from_millis(1));

        let recognized = engine
            .wait_for_result_state(&mut renderer)
            .await
            .expect("retry transition");

        assert_eq!(recognized.state, ResultState::Empty);
        assert_eq!(renderer.calls, 2);
    }

    struct MockSearchRenderer {
        opened: HashMap<String, RenderedDocument>,
        submitted: HashMap<String, RenderedDocument>,
        active: Option<String>,
        current: Option<RenderedDocument>,
        queries: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl SearchRenderer for MockSearchRenderer {
        async fn open(&mut self, url: &Url) -> BrowserResult<RenderedDocument> {
            let page =
                self.opened
                    .get(url.as_str())
                    .cloned()
                    .ok_or_else(|| BrowserError::Operation {
                        stage: "mock open",
                        message: "missing page".to_string(),
                    })?;
            self.active = Some(url.to_string());
            self.current = Some(page.clone());
            Ok(page)
        }

        async fn submit(&mut self, _form: &SearchForm, query: &str) -> BrowserResult<()> {
            self.queries
                .lock()
                .expect("query lock")
                .push(query.to_string());
            let active = self.active.as_ref().expect("active candidate");
            self.current = self.submitted.get(active).cloned();
            Ok(())
        }

        async fn current(&mut self) -> BrowserResult<RenderedDocument> {
            self.current.clone().ok_or_else(|| BrowserError::Operation {
                stage: "mock current",
                message: "missing result".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn malformed_first_candidate_falls_back_to_next_candidate() {
        let first = candidate("https://93.184.216.20/");
        let second = candidate("https://93.184.216.21/");
        let valid_form = include_str!("../tests/fixtures/discovery/valid_candidate.html");
        let malformed_rows = r#"<table id="searchResult"><tr><td>not a magnet</td></tr></table>"#;
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut renderer = MockSearchRenderer {
            opened: HashMap::from([
                (
                    first.candidate.to_string(),
                    document(first.candidate.as_url().as_str(), valid_form),
                ),
                (
                    second.candidate.to_string(),
                    document(second.candidate.as_url().as_str(), valid_form),
                ),
            ]),
            submitted: HashMap::from([
                (
                    first.candidate.to_string(),
                    document(first.candidate.as_url().as_str(), malformed_rows),
                ),
                (
                    second.candidate.to_string(),
                    document(
                        second.candidate.as_url().as_str(),
                        include_str!("../tests/fixtures/search/classic.html"),
                    ),
                ),
            ]),
            active: None,
            current: None,
            queries: Arc::clone(&queries),
        };
        let engine = SearchEngine::for_test(Duration::from_secs(1), Duration::from_millis(1));

        let outcome = engine
            .search_candidates(&mut renderer, &[first, second.clone()], "legal image")
            .await
            .expect("second candidate should succeed");

        assert_eq!(outcome.candidate, second.candidate);
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].stage, SearchFailureStage::ResultParsing);
        assert_eq!(
            queries.lock().expect("query lock").as_slice(),
            ["legal image", "legal image"]
        );
    }

    async fn serve_search_page() -> (Url, Arc<Mutex<Vec<String>>>, JoinHandle<io::Result<()>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind local server");
        let address = listener.local_addr().expect("server address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await?;
                let requests = Arc::clone(&recorded);
                tokio::spawn(async move {
                    let mut buffer = [0_u8; 8192];
                    let count = stream.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..count]);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    requests.lock().expect("request lock").push(path.clone());
                    let body = if path.starts_with("/search?") {
                        include_str!("../tests/fixtures/search/classic.html")
                    } else {
                        include_str!("../tests/fixtures/discovery/valid_candidate.html")
                    };
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
            requests,
            task,
        )
    }

    #[tokio::test]
    #[ignore = "requires a system Chrome/Chromium executable"]
    async fn complete_query_uses_rendered_local_form_without_network_api() {
        if crate::browser::locate_browser(None).is_err() {
            eprintln!("skipping: no system Chrome or Chromium executable");
            return;
        }
        let (url, requests, server) = serve_search_page().await;
        let initial = document(
            url.as_str(),
            include_str!("../tests/fixtures/discovery/valid_candidate.html"),
        );
        let candidate = ValidatedCandidate {
            candidate: normalize_candidate(url.clone()).expect("candidate"),
            rendered_url: url.clone(),
            search_form: validate_candidate_page(&initial).expect("search form"),
        };
        let policy = NavigationPolicy::local_test_pages();
        let settings = BrowserConfig {
            navigation_timeout_seconds: 10,
            selector_timeout_seconds: 10,
            overall_timeout_seconds: 30,
            ..BrowserConfig::default()
        };
        let mut browser = BrowserSession::launch(&settings, policy)
            .await
            .expect("launch browser");
        let engine = SearchEngine::new(Duration::from_secs(10));

        let outcome = engine
            .search_candidates(&mut browser, &[candidate], "legal test image")
            .await
            .expect("rendered search succeeds");

        assert_eq!(outcome.results.len(), 1);
        assert!(requests.lock().expect("request lock").iter().any(|path| {
            path.contains("q=legal+test+image") || path.contains("q=legal%20test%20image")
        }));
        browser.shutdown().await.expect("shutdown browser");
        server.abort();
    }
}
