use anyhow::{Context, Result};
use rss::Channel;
use serde::Deserialize;
use std::collections::HashSet;

const DEFAULT_APIBAY_BASE: &str = "https://apibay.org";
const DEFAULT_RSS_INDEXES: &[RssIndex] = &[RssIndex {
    name: "EZTV",
    search_url: "https://eztvx.to/ezrss.xml?search={query}",
}];

#[derive(Deserialize, Debug, Clone)]
pub struct Torrent {
    pub name: String,
    pub info_hash: String,
    pub seeders: String,
    pub leechers: String,
    pub size: String,
    pub category: String,
    pub source: String,
    magnet_uri: Option<String>,
}

impl Torrent {
    pub fn seeders_u64(&self) -> u64 {
        self.seeders.parse().unwrap_or(0)
    }

    pub fn size_bytes(&self) -> u64 {
        self.size.parse().unwrap_or(0)
    }

    pub fn size_human(&self) -> String {
        let bytes = self.size_bytes();
        if bytes >= 1_073_741_824 {
            format!("{:.2} GB", bytes as f64 / 1_073_741_824.0)
        } else if bytes >= 1_048_576 {
            format!("{:.2} MB", bytes as f64 / 1_048_576.0)
        } else {
            format!("{:.2} KB", bytes as f64 / 1_024.0)
        }
    }

    pub fn magnet(&self) -> String {
        if let Some(magnet_uri) = &self.magnet_uri {
            return magnet_uri.clone();
        }

        let trackers = [
            "udp://tracker.opentrackr.org:1337/announce",
            "udp://open.stealth.si:80/announce",
            "udp://tracker.torrent.eu.org:451/announce",
        ];
        let tr: String = trackers
            .iter()
            .map(|t| format!("&tr={}", urlencoding::encode(t)))
            .collect();
        format!(
            "magnet:?xt=urn:btih:{}&dn={}{}",
            self.info_hash,
            urlencoding::encode(&self.name),
            tr
        )
    }
}

#[derive(Deserialize)]
struct ApiBayTorrent {
    name: String,
    info_hash: String,
    seeders: String,
    leechers: String,
    size: String,
    category: String,
}

impl From<ApiBayTorrent> for Torrent {
    fn from(torrent: ApiBayTorrent) -> Self {
        Self {
            name: torrent.name,
            info_hash: torrent.info_hash,
            seeders: torrent.seeders,
            leechers: torrent.leechers,
            size: torrent.size,
            category: torrent.category,
            source: "APibay".to_string(),
            magnet_uri: None,
        }
    }
}

struct RssIndex {
    name: &'static str,
    search_url: &'static str,
}

pub async fn search(query: &str) -> Result<Vec<Torrent>> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        .build()?;

    let mut results = Vec::new();
    let mut errors = Vec::new();

    for base in apibay_bases() {
        match search_apibay(&client, &base, query).await {
            Ok(torrents) => results.extend(torrents),
            Err(error) => errors.push(format!("{base}: {error:#}")),
        }
    }

    for index in DEFAULT_RSS_INDEXES {
        match search_rss_index(&client, index, query).await {
            Ok(torrents) => results.extend(torrents),
            Err(error) => errors.push(format!("{}: {error:#}", index.name)),
        }
    }

    if results.is_empty() && !errors.is_empty() {
        eprintln!("Search providers failed:\n  {}", errors.join("\n  "));
    }

    // APibay returns a single "no results" sentinel.
    results.retain(|t| t.info_hash != "0000000000000000000000000000000000000000");
    dedupe_by_hash(&mut results);
    results.sort_by(|a, b| b.seeders_u64().cmp(&a.seeders_u64()));
    Ok(results)
}

async fn search_apibay(client: &reqwest::Client, base: &str, query: &str) -> Result<Vec<Torrent>> {
    let url = format!(
        "{}/q.php?q={}&cat=0",
        base.trim_end_matches('/'),
        urlencoding::encode(query)
    );
    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("request failed for {url}"))?
        .error_for_status()
        .with_context(|| format!("bad status for {url}"))?;
    let torrents: Vec<ApiBayTorrent> = response
        .json()
        .await
        .with_context(|| format!("invalid JSON from {url}"))?;
    Ok(torrents.into_iter().map(Torrent::from).collect())
}

async fn search_rss_index(
    client: &reqwest::Client,
    index: &RssIndex,
    query: &str,
) -> Result<Vec<Torrent>> {
    let url = index
        .search_url
        .replace("{query}", &urlencoding::encode(query));
    let bytes = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("request failed for {url}"))?
        .error_for_status()
        .with_context(|| format!("bad status for {url}"))?
        .bytes()
        .await
        .with_context(|| format!("failed reading RSS from {url}"))?;
    let channel =
        Channel::read_from(&bytes[..]).with_context(|| format!("invalid RSS from {url}"))?;

    Ok(channel
        .items()
        .iter()
        .filter_map(|item| torrent_from_rss_item(index.name, item))
        .collect())
}

fn torrent_from_rss_item(source: &str, item: &rss::Item) -> Option<Torrent> {
    let name = item
        .extensions()
        .get("torrent")
        .and_then(|extensions| extension_value(extensions, "fileName"))
        .or_else(|| item.title().map(ToString::to_string))?;
    let magnet_uri = item
        .extensions()
        .get("torrent")
        .and_then(|extensions| extension_value(extensions, "magnetURI"));
    let info_hash = item
        .extensions()
        .get("torrent")
        .and_then(|extensions| extension_value(extensions, "infoHash"))
        .or_else(|| {
            magnet_uri
                .as_deref()
                .and_then(info_hash_from_magnet)
                .map(str::to_string)
        })?;
    let size = item
        .extensions()
        .get("torrent")
        .and_then(|extensions| extension_value(extensions, "contentLength"))
        .or_else(|| {
            item.enclosure()
                .map(|enclosure| enclosure.length().to_string())
        })
        .unwrap_or_else(|| "0".to_string());
    let seeders = item
        .extensions()
        .get("torrent")
        .and_then(|extensions| extension_value(extensions, "seeds"))
        .unwrap_or_else(|| "0".to_string());
    let leechers = item
        .extensions()
        .get("torrent")
        .and_then(|extensions| extension_value(extensions, "peers"))
        .unwrap_or_else(|| "0".to_string());

    Some(Torrent {
        name,
        info_hash,
        seeders,
        leechers,
        size,
        category: item
            .categories()
            .first()
            .map_or_else(String::new, |category| category.name().to_string()),
        source: source.to_string(),
        magnet_uri,
    })
}

fn extension_value(
    extensions: &std::collections::BTreeMap<String, Vec<rss::extension::Extension>>,
    key: &str,
) -> Option<String> {
    extensions
        .get(key)?
        .first()?
        .value()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn info_hash_from_magnet(magnet: &str) -> Option<&str> {
    magnet.split('&').find_map(|part| {
        part.strip_prefix("magnet:?xt=urn:btih:")
            .or_else(|| part.strip_prefix("xt=urn:btih:"))
    })
}

fn apibay_bases() -> Vec<String> {
    std::env::var("PBTDL_APIBAY_BASES")
        .ok()
        .map(|bases| {
            bases
                .split(',')
                .map(str::trim)
                .filter(|base| !base.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_else(|| vec![DEFAULT_APIBAY_BASE.to_string()])
}

fn dedupe_by_hash(results: &mut Vec<Torrent>) {
    let mut seen = HashSet::new();
    results.retain(|torrent| seen.insert(torrent.info_hash.to_ascii_lowercase()));
}

// Inline urlencoding to avoid an extra dep — percent-encode the string
mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut out = String::new();
        for byte in s.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(byte as char)
                }
                b' ' => out.push('+'),
                b => out.push_str(&format!("%{:02X}", b)),
            }
        }
        out
    }
}
