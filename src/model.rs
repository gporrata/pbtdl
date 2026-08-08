use data_encoding::BASE32_NOPAD;
use std::fmt;
use std::str::FromStr;
use thiserror::Error;
use url::Url;

const INFO_HASH_BYTES: usize = 20;
const INFO_HASH_HEX_LENGTH: usize = INFO_HASH_BYTES * 2;
const INFO_HASH_BASE32_LENGTH: usize = 32;

/// A canonical lower-case hexadecimal BitTorrent v1 info hash.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InfoHash(String);

impl InfoHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InfoHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for InfoHash {
    type Err = MagnetError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let bytes = if value.len() == INFO_HASH_HEX_LENGTH {
            decode_hex(value)?
        } else if value.len() == INFO_HASH_BASE32_LENGTH {
            BASE32_NOPAD
                .decode(value.to_ascii_uppercase().as_bytes())
                .map_err(|_| MagnetError::InvalidInfoHash)?
        } else {
            return Err(MagnetError::InvalidInfoHash);
        };

        if bytes.len() != INFO_HASH_BYTES {
            return Err(MagnetError::InvalidInfoHash);
        }

        Ok(Self(encode_hex(&bytes)))
    }
}

/// A parsed magnet URI containing a supported BitTorrent v1 identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagnetUri {
    url: Url,
    info_hash: InfoHash,
}

impl MagnetUri {
    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }

    pub fn info_hash(&self) -> &InfoHash {
        &self.info_hash
    }
}

impl fmt::Display for MagnetUri {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for MagnetUri {
    type Err = MagnetError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let url = Url::parse(value).map_err(|_| MagnetError::InvalidUri)?;
        if url.scheme() != "magnet" {
            return Err(MagnetError::UnsupportedScheme);
        }

        let info_hash = url
            .query_pairs()
            .filter(|(key, _)| key.eq_ignore_ascii_case("xt"))
            .find_map(|(_, value)| {
                value
                    .strip_prefix("urn:btih:")
                    .or_else(|| value.strip_prefix("URN:BTIH:"))
                    .map(InfoHash::from_str)
            })
            .transpose()?
            .ok_or(MagnetError::MissingInfoHash)?;

        Ok(Self { url, info_hash })
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MagnetError {
    #[error("magnet URI is malformed")]
    InvalidUri,
    #[error("URI scheme must be magnet")]
    UnsupportedScheme,
    #[error("magnet URI has no supported urn:btih identity")]
    MissingInfoHash,
    #[error("BitTorrent info hash must be 40 hexadecimal or 32 base32 characters")]
    InvalidInfoHash,
}

/// Normalized rendered search result. Optional page metadata remains optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentResult {
    pub name: String,
    pub magnet: MagnetUri,
    pub seeders: u64,
    pub leechers: Option<u64>,
    pub size_bytes: Option<u64>,
    pub category: Option<String>,
    pub source_host: String,
}

impl TorrentResult {
    pub fn info_hash(&self) -> &InfoHash {
        self.magnet.info_hash()
    }
}

/// Truncate by Unicode scalar values, never by byte offsets.
pub fn truncate_display(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn decode_hex(value: &str) -> Result<Vec<u8>, MagnetError> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_value(pair[0]).ok_or(MagnetError::InvalidInfoHash)?;
            let low = hex_value(pair[1]).ok_or(MagnetError::InvalidInfoHash)?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX_HASH: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn parses_and_canonicalizes_hex_info_hash() {
        let magnet = format!("magnet:?dn=Example&xt=urn:btih:{}", HEX_HASH.to_uppercase());
        let parsed = MagnetUri::from_str(&magnet).expect("valid magnet");

        assert_eq!(parsed.info_hash().as_str(), HEX_HASH);
    }

    #[test]
    fn parses_base32_info_hash() {
        let bytes = decode_hex(HEX_HASH).expect("fixture hash");
        let base32 = BASE32_NOPAD.encode(&bytes);
        let parsed = MagnetUri::from_str(&format!("magnet:?xt=urn:btih:{base32}"))
            .expect("valid base32 magnet");

        assert_eq!(parsed.info_hash().as_str(), HEX_HASH);
    }

    #[test]
    fn rejects_missing_or_invalid_download_identity() {
        for value in [
            "https://example.test/file.torrent",
            "magnet:?dn=no-identity",
            "magnet:?xt=urn:btih:not-a-hash",
            "magnet:?xt=urn:btmh:0123456789abcdef",
        ] {
            assert!(MagnetUri::from_str(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn truncates_unicode_on_character_boundaries() {
        assert_eq!(truncate_display("Rust 🦀 résumé", 6), "Rust 🦀");
        assert_eq!(truncate_display("短い", 20), "短い");
    }
}
