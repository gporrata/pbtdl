use anyhow::Result;
use serde::Deserialize;

const API_BASE: &str = "https://apibay.org/q.php";

#[derive(Deserialize, Debug, Clone)]
pub struct Torrent {
    pub name: String,
    pub info_hash: String,
    pub seeders: String,
    pub leechers: String,
    pub size: String,
    pub category: String,
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

pub async fn search(query: &str) -> Result<Vec<Torrent>> {
    let url = format!("{}?q={}&cat=0", API_BASE, urlencoding::encode(query));
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        .build()?;
    let mut results: Vec<Torrent> = client.get(&url).send().await?.json().await?;
    // APibay returns a single "no results" sentinel
    results.retain(|t| t.info_hash != "0000000000000000000000000000000000000000");
    results.sort_by(|a, b| b.seeders_u64().cmp(&a.seeders_u64()));
    Ok(results)
}

// Inline urlencoding to avoid an extra dep — percent-encode the string
mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut out = String::new();
        for byte in s.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
                | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
                b' ' => out.push('+'),
                b => out.push_str(&format!("%{:02X}", b)),
            }
        }
        out
    }
}
