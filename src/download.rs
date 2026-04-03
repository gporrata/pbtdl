use anyhow::{bail, Result};
use std::path::Path;
use std::process::Command;

/// Try to find a supported downloader on PATH.
fn find_downloader() -> Option<&'static str> {
    for prog in &["aria2c", "transmission-cli", "qbittorrent-nox"] {
        if Command::new("which")
            .arg(prog)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(prog);
        }
    }
    None
}

pub fn download(magnet: &str, output_dir: &Path) -> Result<()> {
    let downloader = find_downloader().ok_or_else(|| {
        anyhow::anyhow!(
            "No supported downloader found. Install aria2c, transmission-cli, or qbittorrent-nox."
        )
    })?;

    let dir = output_dir.to_string_lossy();
    let status = match downloader {
        "aria2c" => Command::new("aria2c")
            .args(["--dir", &dir, magnet])
            .status()?,
        "transmission-cli" => Command::new("transmission-cli")
            .args(["--download-dir", &dir, magnet])
            .status()?,
        "qbittorrent-nox" => Command::new("qbittorrent-nox")
            .args(["--save-path", &dir, magnet])
            .status()?,
        _ => unreachable!(),
    };

    if !status.success() {
        bail!("{} exited with status {}", downloader, status);
    }
    Ok(())
}
