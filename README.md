# pbtdl

`pbtdl` is a Rust command-line application for Ubuntu that discovers a working Pirate Bay-compatible proxy, searches it through a real Chromium browser, presents normalized torrent results, and hands the selected magnet link to a locally installed torrent client.

The application is intended only for content the user is legally permitted to download. It does not attempt to determine copyright status or grant permission; choosing lawful content remains the user's responsibility.

## Product shape

The first release is a local, single-user CLI. It preserves the useful interaction model from the earlier `master_old` implementation:

- A search query is supplied on the command line.
- Results are normalized and ranked by seeder count by default.
- The user normally chooses from an interactive result list.
- The number of displayed results and the output directory are configurable.
- An explicit automatic-selection mode may choose the highest-ranked result.
- The selected magnet is downloaded by a supported torrent client already installed on the machine.

There is no web UI, hosted service, or built-in BitTorrent implementation.

## End-to-end design

```text
Load or create configuration
        |
Discover proxy candidates from configured web pages and seed URLs
        |
Validate candidates in isolated headless Chromium
        |
Search the first usable candidate and parse its rendered results
        |
Normalize, deduplicate, rank, and display torrents
        |
Validate the selected magnet and obtain user selection or --auto choice
        |
Invoke a local torrent client and report the outcome
```

If a candidate is unavailable, malformed, protected by a CAPTCHA or similar challenge, or does not behave like the expected site, it is skipped and the next candidate is tried.

## Search and discovery decisions

Torrent search-index APIs are deliberately excluded. The earlier APibay JSON and RSS implementations proved unreliable and will not be restored. Discovery, validation, searching, and result extraction operate on browser-rendered web pages.

Browser behavior is deterministic rather than LLM-driven. Each supported page shape has ordered selectors and structural fallbacks. When a known selector fails, the application tries bounded DOM heuristics such as locating candidate links, search forms, result rows, and magnet anchors by their attributes and surrounding structure. If those strategies cannot establish confidence, the page is rejected instead of asking an AI agent to improvise.

Proxy discovery starts from user-editable source pages and optional seed candidates. Candidate URLs are normalized and deduplicated before validation. A recently healthy candidate may be cached to speed up later runs, but it is revalidated before use. Cache data is operational state and is kept outside the configuration file.

The application does not solve CAPTCHAs, bypass bot protection, or circumvent authentication. Encountering any of those conditions causes the candidate to be skipped.

## Browser decisions

The application controls a system-installed Chrome or Chromium browser. It does not bundle or silently download a browser. The executable is auto-detected from common Ubuntu locations, with a configuration override for nonstandard installations.

Chromium runs headlessly by default with:

- A new temporary profile for each run.
- No access to the user's normal cookies, extensions, history, or credentials.
- Browser-managed downloads disabled; Chromium is used only for discovery and search.
- Pop-ups and unexpected top-level navigation rejected.
- Each rendered document transition intercepted before it is continued, including HTTP redirects.
- Bounded navigation, selector, redirect, document-size, and overall browser-workflow limits.
- Temporary browser state cleaned up after normal completion or failure.

Headful mode may be exposed as a troubleshooting option, but it does not change the deterministic automation model.

## Configuration decisions

Persistent settings live at `$XDG_CONFIG_HOME/pbtdl/pbtdl.toml`. When `XDG_CONFIG_HOME` is unset, the path is `~/.config/pbtdl/pbtdl.toml`.

On its first run, the application creates the parent directory and a commented default configuration containing safe browser defaults, maintained discovery starting points, search defaults, and downloader defaults. Creation must not overwrite an existing file. If an existing configuration is malformed, the application reports the path and parsing problem and leaves the file unchanged.

The configuration distinguishes between:

- Discovery source pages, whose rendered links may contain proxy candidates.
- Seed candidates, which are tested directly.
- Browser executable and timeout settings.
- Search result and filtering defaults.
- Downloader selection, output directory, and seeding policy.

Command-line arguments take precedence over configuration values. The generated configuration includes a schema version so future releases can detect incompatible changes instead of silently misinterpreting settings.

Discovered proxy health and expiry information belongs under `$XDG_CACHE_HOME/pbtdl`, falling back to `~/.cache/pbtdl`; it is never mixed into the user-maintained TOML file.

## Search result model

Results from different supported page layouts are converted into one typed representation containing, when available:

- Display name.
- Validated BitTorrent info hash and magnet URI.
- Seeder and leecher counts.
- Content size.
- Category.
- Source host.

Info hashes, numeric fields, URLs, and magnets are validated during normalization. Results with the same info hash are deduplicated. Missing optional metadata may be displayed as unknown, but a result without a valid magnet or info hash cannot be downloaded.

Seeder count is the initial default ordering because it matches the behavior of `master_old`. Interactive selection remains the default because ranking does not establish relevance or legal status. Automatic selection is an explicit user choice and fails cleanly when filters leave no eligible results.

## Downloader decisions

The initial release delegates BitTorrent transfers to local executables, following the `master_old` approach. It does not use a search-index API and does not need a remote download service. The supported foreground adapter is:

- `aria2c`, preferred when available.

`transmission-cli` does not expose a reliable stop-seeding-on-completion option, and `qbittorrent-nox` is a long-running service rather than a foreground per-download process. They are recognized for actionable diagnostics but are not advertised as supported in this release. Automatic client detection therefore selects `aria2c`; an explicit incompatible selection fails before launch. Each client is invoked directly with an argument array, never through a shell command. The selected magnet is therefore treated as data rather than executable command text.

The default workflow is foreground-oriented: `pbtdl` waits for the downloader outcome, propagates failures, and reports what completed. Clients must implement consistent download and no-post-completion-seeding semantics to be advertised as supported. A future enqueue mode may use a local daemon interface, but local downloader APIs are not required for the first release.

The output directory is created or validated before starting a client. Browser pages never control the destination, executable path, or arbitrary downloader flags.

## Safety boundaries

Every discovery page, proxy, rendered document, result, and magnet is untrusted input. The application therefore:

- Permits only configured HTTP or HTTPS discovery entry points, with secure transport preferred by default.
- Rejects navigation to local files, loopback addresses, private networks, and unsupported schemes.
- Rechecks top-level redirects before accepting a candidate.
- Allows at most five HTTP redirect hops per rendered page and rejects every document hop that resolves to a local, private, or reserved destination.
- Limits a rendered HTML document to 2 MiB, a query to 512 characters and 2 KiB, and extracted titles to 512 characters. Terminal controls and bidirectional formatting marks are removed from untrusted display text.
- Limits configured and processed source pages, candidates, forms, anchors, result rows, row links, cache entries, and filesystem attribution walks.
- Requires a well-formed `magnet:` URI with a supported `urn:btih` identifier.
- Does not execute commands through a shell.
- Rejects a filesystem root as an output directory and does not follow symlinks while attributing newly created files.
- Does not reuse the user's browser profile or persist site cookies.
- Does not automatically open downloaded content.

These controls reduce exposure to malicious proxy pages but do not make an untrusted site inherently safe.

Some boundaries necessarily depend on the programs `pbtdl` owns but does not implement. Chromium remains responsible for its renderer sandbox, TLS implementation, DNS-to-connection behavior, and non-document subresource requests; `pbtdl` validates document destinations, but cannot make DNS rebinding impossible. The selected torrent client remains responsible for validating torrent metadata, containing payload paths beneath its configured directory, honoring `--seed-time=0`, and enforcing filesystem permissions. `pbtdl` invokes only the documented `aria2c` contract, waits in the foreground, and terminates its owned child on interruption. Transfer duration is intentionally user-controlled rather than covered by the browser workflow deadline.

## Component boundaries

The implementation is divided into replaceable responsibilities:

- **Configuration:** creates defaults, merges CLI overrides, and validates settings.
- **Browser session:** launches and owns isolated Chromium processes.
- **Discovery:** extracts, normalizes, caches, and orders proxy candidates.
- **Candidate validation:** detects supported page shapes and blocked or deceptive pages.
- **Search:** submits queries and extracts rendered result rows.
- **Domain model:** validates, deduplicates, and ranks normalized torrent results.
- **Selection:** renders interactive choices and applies explicit automatic selection.
- **Downloader:** detects clients, builds safe invocations, and reports completion.

Browser-specific details do not leak into downloader code, and client-specific behavior does not leak into discovery or search. This separation allows selectors and supported clients to evolve independently.

## Reliability and testing decisions

Automated tests do not depend on live proxy sites and never download torrent content. Parser and fallback behavior is tested with sanitized HTML fixtures and local test pages. Downloader behavior is tested with fake executables that record their argument arrays. Configuration tests use isolated temporary XDG directories.

An opt-in live smoke test may verify that configured discovery pages can still be rendered and that at least one candidate has a recognizable search form. Live failures are diagnostic signals, not part of the deterministic unit-test suite.

All normal commits are expected to leave formatting, compilation, linting, and tests passing. Failures should identify which stage and candidate failed without exposing unrelated browser or environment data.

## Explicit non-goals

The initial application will not:

- Use APibay, RSS, JSON, or another torrent search-index API.
- Use an LLM or autonomous browser agent.
- Defeat CAPTCHAs, bot protection, authentication, or access controls.
- Bundle Chromium or modify the user's normal browser profile.
- Implement the BitTorrent protocol itself.
- Determine whether a torrent is legally downloadable.
- Provide a hosted service, web interface, or multi-user daemon.
