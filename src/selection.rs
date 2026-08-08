//! Terminal result presentation and deterministic selection.

use crate::model::{TorrentResult, sanitize_display_text};
use anyhow::{Context, Result, bail};
use dialoguer::{Select, theme::ColorfulTheme};

pub trait ResultChooser {
    fn choose(&mut self, results: &[TorrentResult], automatic: bool) -> Result<usize>;
}

#[derive(Debug, Clone)]
pub struct TerminalChooser {
    max_title_characters: usize,
}

impl TerminalChooser {
    pub fn new(max_title_characters: usize) -> Self {
        Self {
            max_title_characters,
        }
    }
}

impl ResultChooser for TerminalChooser {
    fn choose(&mut self, results: &[TorrentResult], automatic: bool) -> Result<usize> {
        if results.is_empty() {
            bail!("no eligible torrent results were found");
        }
        if automatic {
            return Ok(0);
        }
        let items: Vec<_> = results
            .iter()
            .map(|result| format_result(result, self.max_title_characters))
            .collect();
        Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Select a torrent")
            .items(&items)
            .default(0)
            .interact_opt()
            .context("interactive selection failed")?
            .ok_or_else(|| anyhow::anyhow!("torrent selection was cancelled"))
    }
}

pub fn format_result(result: &TorrentResult, max_title_characters: usize) -> String {
    let title = truncate_with_ellipsis(
        &sanitize_display_text(&result.name, max_title_characters.saturating_add(1)),
        max_title_characters,
    );
    let seeders = result
        .seeders
        .map_or_else(|| "?".to_string(), |value| value.to_string());
    let leechers = result
        .leechers
        .map_or_else(|| "?".to_string(), |value| value.to_string());
    let category = result
        .category
        .as_deref()
        .map(|value| sanitize_display_text(value, 128))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let source = sanitize_display_text(&result.source_host, 253);
    let size = result
        .size_bytes
        .map_or_else(|| "unknown".to_string(), |value| human_size(Some(value)));
    format!(
        "{title} | seeders {seeders} | leechers {leechers} | {category} | {} | {size}",
        source
    )
}

pub fn human_size(bytes: Option<u64>) -> String {
    let Some(bytes) = bytes else {
        return "unknown".to_string();
    };
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = UNITS[0];
    for candidate in &UNITS[1..] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = candidate;
    }
    if unit == "B" {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {unit}")
    }
}

pub fn truncate_with_ellipsis(value: &str, max_characters: usize) -> String {
    let count = value.chars().count();
    if count <= max_characters {
        return value.to_string();
    }
    if max_characters == 0 {
        return String::new();
    }
    if max_characters == 1 {
        return "…".to_string();
    }
    let mut truncated: String = value.chars().take(max_characters - 1).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MagnetUri;
    use std::str::FromStr;

    fn result() -> TorrentResult {
        TorrentResult {
            name: "Ubuntu 🐧 résumé image".to_string(),
            magnet: MagnetUri::from_str(
                "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567",
            )
            .expect("magnet"),
            seeders: Some(42),
            leechers: None,
            size_bytes: Some(1_610_612_736),
            category: Some("Applications".to_string()),
            source_host: "proxy.example".to_string(),
        }
    }

    #[test]
    fn formats_all_available_and_unknown_fields() {
        let formatted = format_result(&result(), 14);

        assert!(formatted.contains("Ubuntu 🐧 résu…"));
        assert!(formatted.contains("seeders 42"));
        assert!(formatted.contains("leechers ?"));
        assert!(formatted.contains("Applications"));
        assert!(formatted.contains("proxy.example"));
        assert!(formatted.contains("1.50 GiB"));
    }

    #[test]
    fn unicode_truncation_is_safe_at_small_limits() {
        assert_eq!(truncate_with_ellipsis("🦀Rust", 3), "🦀R…");
        assert_eq!(truncate_with_ellipsis("🦀", 1), "🦀");
        assert_eq!(truncate_with_ellipsis("🦀Rust", 1), "…");
        assert_eq!(truncate_with_ellipsis("🦀Rust", 0), "");
    }

    #[test]
    fn formats_human_sizes() {
        assert_eq!(human_size(None), "unknown");
        assert_eq!(human_size(Some(512)), "512 B");
        assert_eq!(human_size(Some(1_048_576)), "1.00 MiB");
    }

    #[test]
    fn automatic_selection_chooses_the_first_ranked_result() {
        let mut chooser = TerminalChooser::new(80);
        assert_eq!(chooser.choose(&[result()], true).expect("automatic"), 0);
    }

    #[test]
    fn formatting_does_not_emit_page_supplied_terminal_controls() {
        let mut hostile = result();
        hostile.name = "normal\u{1b}[2J title".to_string();
        hostile.category = Some("Apps\u{202e}evil".to_string());

        let formatted = format_result(&hostile, 80);

        assert!(!formatted.contains('\u{1b}'));
        assert!(!formatted.contains('\u{202e}'));
    }
}
