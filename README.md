# pbtdl

A Rust CLI that searches The Pirate Bay, shows an interactive list of results, and downloads the selected torrent.

## Requirements

One of the following downloaders must be installed and on your `PATH`:

- [`aria2c`](https://aria2.github.io/) (recommended)
- `transmission-cli`
- `qbittorrent-nox`

## Installation

```sh
cargo install --path .
```

## Usage

```
pbtdl <query> [OPTIONS]
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `-o, --output <dir>` | `.` | Directory to save downloaded files |
| `-r, --results <n>` | `10` | Number of results to show in the selection list |
| `--auto` | — | Skip interactive selection; download the top result automatically |

### Examples

```sh
# Interactive selection from top 10 results
pbtdl "ubuntu 24.04"

# Show top 5 results to choose from, save to ~/Downloads
pbtdl "ubuntu 24.04" -r 5 -o ~/Downloads

# Non-interactive: grab the top result immediately
pbtdl "ubuntu 24.04" --auto
```

## How It Works

1. **Search** — queries public torrent indexes and sorts results by seeder count descending.
2. **Select** — presents an interactive list (via [dialoguer](https://github.com/console-rs/dialoguer)) so you can pick the right release. Use `--auto` to skip this step.
3. **Download** — constructs a magnet URI from the torrent's info hash and hands it off to your installed downloader (`aria2c`, `transmission-cli`, or `qbittorrent-nox`).

## Search Indexes

`pbtdl` is designed to use public HTTP indexes directly, so users do not need to run a separate indexer service.

Built-in providers:

- APibay-compatible Pirate Bay API: JSON results from `/q.php`.
- EZTV RSS: TV-oriented RSS results with magnet metadata.

If `https://apibay.org` is blocked or down, set one or more APibay-compatible base URLs:

```sh
PBTDL_APIBAY_BASES="https://apibay.example,https://another.example" pbtdl "ubuntu"
```

Good candidates for future built-in providers are public indexes with stable RSS or JSON search endpoints that include either a magnet link or info hash. Scrape-only sites are intentionally avoided because they tend to break behind bot protection and markup changes.

## Project Structure

```
src/
  main.rs      — CLI argument parsing and orchestration
  search.rs    — Pirate Bay API calls, result parsing, magnet URI construction
  download.rs  — Downloader detection and invocation
```
