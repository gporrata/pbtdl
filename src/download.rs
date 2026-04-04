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
            .args(["--seed-time=0", "--dir", &dir, magnet])
            .status()?,
        "transmission-cli" => Command::new("transmission-cli")
            .args(["--seedratio", "0", "--download-dir", &dir, magnet])
            .status()?,
        "qbittorrent-nox" => Command::new("qbittorrent-nox")
            .args(["--save-path", &dir, "--add-paused", magnet])
            .status()?,
        _ => unreachable!(),
    };

    if !status.success() {
        bail!("{} exited with status {}", downloader, status);
    }

    print_downloaded_files(output_dir);
    Ok(())
}

fn print_downloaded_files(dir: &Path) {
    let mut files: Vec<(std::path::PathBuf, u64)> = Vec::new();
    collect_files(dir, &mut files);
    if files.is_empty() {
        println!("No files found in output directory.");
        return;
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    println!("\nDownloaded files:");
    for (path, size) in &files {
        let rel = path.strip_prefix(dir).unwrap_or(path);
        println!("  {}  ({})", rel.display(), human_size(*size));
    }
}

fn collect_files(dir: &Path, out: &mut Vec<(std::path::PathBuf, u64)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else if let Ok(meta) = entry.metadata() {
            out.push((path, meta.len()));
        }
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = UNITS[0];
    for u in &UNITS[1..] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = u;
    }
    if unit == "B" {
        format!("{} B", bytes)
    } else {
        format!("{:.1} {}", value, unit)
    }
}
