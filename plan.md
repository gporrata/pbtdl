# pbtdl implementation plan

This plan builds the browser-driven design in `README.md` as a sequence of reviewable, buildable commits. `master_old` is a behavioral reference for CLI presentation and local downloader invocation only. Its APibay and RSS search implementations must not be copied into the new application.

## Execution prologue

Use the following prompt to begin an implementation run:

```text
Build pbtdl by executing plan.md from top to bottom.

Before changing anything, read README.md, about.md, plan.md, all applicable AGENTS.md files, and inspect master_old with git show without checking it out. Treat README.md as the canonical design and master_old only as a behavioral reference for the CLI and downloader adapters. Preserve unrelated user changes and never commit target/ or other generated artifacts.

Implement deterministic Chrome/Chromium automation. Do not use APibay, RSS, JSON endpoints, hidden page APIs, an LLM, or any other torrent search-index API. Search and proxy discovery must operate on rendered pages. Skip candidates that are blocked, challenged, malformed, or unsupported; do not bypass CAPTCHAs or access controls.

Follow the commit sequence and use each specified Conventional Commit message exactly. Keep every commit focused and leave the repository compiling with its relevant tests passing. Add tests with local fixtures, local HTTP pages, temporary XDG directories, and fake downloader executables; automated tests must not rely on live proxy sites or download torrent content.

Before each commit, review the diff and run the checks listed for that step. Stage intended files by explicit path rather than using broad commands such as `git add .` or `git add -A`. Inspect `git diff --cached --name-only` before committing and unstage any generated artifact. At the end, run the complete validation suite, inspect the full branch diff and tracked-file list, and report implemented behavior, test evidence, and any remaining limitations. Do not push or open a pull request unless explicitly asked.
```

## Repository hygiene invariant

Rust and browser tooling may generate files locally, but generated artifacts must never be staged or committed. This invariant applies to every commit in the plan, not only the final cleanup:

- Add `/target/` to the root `.gitignore` before running the first Cargo build.
- Ignore temporary Chromium profiles, runtime caches, coverage output, flamegraphs, logs, downloaded files, and editor or operating-system metadata.
- Keep test-generated files inside temporary directories that clean themselves up; do not write fixtures or snapshots into the repository unless they are intentional, reviewed test inputs.
- Stage files by explicit path. Do not use broad staging commands that can accidentally capture generated output.
- Before every commit, inspect both `git status --short --ignored` and `git diff --cached --name-only`.
- Before declaring completion, inspect `git ls-files` and fail the hygiene check if any tracked path is under `target/` or matches a known build, coverage, browser-profile, cache, log, or download artifact.
- Treat `Cargo.lock` as source-controlled application metadata, not as a disposable build artifact; it should be committed intentionally when dependencies change.

If an artifact is already tracked, remove it from the Git index without deleting unrelated user data, add the appropriate ignore rule, and verify the staged deletion before continuing. Never delete an existing untracked artifact merely to make `git status` clean.

## Commit 1 — establish the design baseline

Commit message:

```text
docs: define the browser-driven application design
```

Changes:

- Add `about.md`, `README.md`, and `plan.md` as the documented project baseline.
- Ensure `README.md` is internally consistent about deterministic browser automation, configuration, and local downloader processes.
- Add or update `.gitignore` before any Cargo command so `/target/`, local browser profiles, caches, coverage output, logs, downloads, and editor files cannot be committed accidentally.
- Do not introduce application code in this commit.

Verification:

- Review the staged file list and confirm that `target/` is absent.
- Run `git check-ignore target/` when the directory exists and confirm it is ignored by the repository rule.
- Search `README.md` and `plan.md` for contradictory promises of an index API or LLM-driven browser; retain `about.md` as the original brief, with `README.md` recording the later clarifications.

## Commit 2 — scaffold the Rust CLI and domain model

Commit message:

```text
chore: scaffold the Rust CLI application
```

Changes:

- Create the Cargo package and initial module boundaries for configuration, browser control, discovery, search, selection, and downloading.
- Add the asynchronous runtime, CLI parsing, error handling, serialization, TOML, URL, and terminal dependencies required by the design.
- Define the top-level CLI contract based on `master_old`: query, output directory, result count, and explicit automatic-selection mode.
- Add a typed torrent result model with validated numeric fields and a distinct validated magnet type.
- Reject zero result limits and implement Unicode-safe display truncation from the beginning.
- Keep all operational modules as compiling stubs; do not add API-backed search code.

Verification:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo run -- --help
```

## Commit 3 — create and load XDG configuration

Commit message:

```text
feat(config): create and load XDG configuration
```

Changes:

- Resolve `$XDG_CONFIG_HOME/pbtdl/pbtdl.toml`, falling back to `~/.config/pbtdl/pbtdl.toml`.
- Define a versioned, typed configuration covering discovery pages, seed candidates, browser settings, search defaults, and downloader settings.
- Create a commented and usable default file when none exists, without overwriting a file created concurrently.
- Include a small maintained set of browser-rendered discovery starting points and clearly separate them from direct proxy seeds. Verify that defaults are ordinary web pages rather than API endpoints before committing them.
- Return contextual errors for unreadable, malformed, or unsupported configuration while preserving the original file byte-for-byte.
- Merge settings in the order built-in defaults, configuration file, then CLI overrides.
- Resolve `$XDG_CACHE_HOME/pbtdl`, falling back to `~/.cache/pbtdl`, without storing cache state yet.
- Provide an explicit configuration-path override to make troubleshooting and tests deterministic.

Tests:

- Missing configuration creates the expected directory and template.
- Existing configuration is loaded and never overwritten.
- Malformed TOML and unsupported schema versions produce actionable errors.
- XDG environment and fallback paths resolve correctly in isolated test environments.
- CLI values override configuration without changing the file.

Verification: run formatting, Clippy with warnings denied, all tests, and a temporary-directory first-run smoke test.

## Commit 4 — manage isolated Chromium sessions

Commit message:

```text
feat(browser): launch isolated Chromium sessions
```

Changes:

- Add a maintained Rust Chrome DevTools Protocol client and encapsulate it behind a browser-session interface.
- Locate common Ubuntu Chrome and Chromium executables, honoring the configured executable first.
- Launch headless by default with a unique temporary user-data directory and no connection to the user's normal profile.
- Keep the browser event handler alive for the complete session and shut down the child process on success, error, or cancellation.
- Apply navigation and overall timeouts.
- Disable browser-managed downloads, reject unexpected new windows, and expose headful mode only as a diagnostic configuration option.
- Produce concise errors for a missing executable, launch failure, premature browser exit, and timeout.

Tests:

- Unit-test executable selection and launch configuration without requiring a browser.
- Add an opt-in browser smoke test against a local static page through an explicitly injected test-only navigation policy; production policy continues to reject loopback and private-network destinations.
- Verify temporary profile ownership and cleanup behavior.

Verification: run the standard Rust checks; when Chromium is installed, run the opt-in local smoke test in both headless and headful modes.

## Commit 5 — discover and validate proxy candidates

Commit message:

```text
feat(discovery): find and validate proxy candidates
```

Changes:

- Render each configured discovery source page and extract candidate links using ordered source-specific selectors followed by bounded structural anchor heuristics.
- Combine discovered links with configured seed candidates, normalize URLs, remove fragments, and deduplicate hosts.
- Accept only permitted HTTP(S) navigation and reject unsupported schemes, credentials in URLs, loopback targets, private-network targets, and disallowed redirects.
- Cap source pages, candidates, redirects, page time, and extraction work.
- Validate a candidate by recognizing a supported Pirate Bay-compatible page shape and usable search form rather than trusting branding or page titles alone.
- Detect common challenge, CAPTCHA, and access-denied states and skip those candidates without attempting bypasses.
- Try candidates in deterministic order and return structured failure summaries when none are usable.
- Persist bounded proxy health data in the XDG cache and revalidate cached successes before use.

Tests:

- Use local discovery-page fixtures covering known selectors, generic link fallback, duplicates, malformed URLs, redirects, and excessive candidate lists.
- Cover blocked, CAPTCHA, deceptive, timed-out, and valid candidate pages.
- Cover cache expiry, corruption, revalidation, and fallback ordering.
- Assert that local/private navigation and non-HTTP schemes are rejected before browser navigation.

Verification: run the standard checks and an opt-in local-browser integration test, using the test-only navigation policy, containing one failed candidate followed by one valid candidate.

## Commit 6 — search rendered pages and normalize results

Commit message:

```text
feat(search): automate proxy searches and parse results
```

Changes:

- Submit queries through the validated rendered search form and wait for a bounded, recognizable result state.
- Support ordered page-shape selectors followed by deterministic form and table/list heuristics.
- Extract names, magnet links, info hashes, seeders, leechers, sizes, categories, and source hosts without calling page or network APIs.
- Parse magnets with a URL-aware implementation and accept only supported `urn:btih` identifiers.
- Normalize numeric and size fields, preserve unknown optional metadata, and reject results lacking a valid download identity.
- Deduplicate results by canonical info hash and order them by seeder count with deterministic tie-breaking.
- If a validated candidate fails during search or parsing, close it and try the next candidate.

Tests:

- Add sanitized HTML fixtures for each supported page shape and every selector fallback.
- Cover Unicode titles, absent fields, malformed numbers, invalid magnets, base16/base32 hashes, duplicates, zero results, and misleading magnet-like links.
- Exercise a complete query against local rendered test pages without internet access.

Verification: run the standard checks and confirm that source code contains no APibay, RSS, index JSON, or hidden endpoint integration.

## Commit 7 — connect search to interactive selection

Commit message:

```text
feat(cli): add interactive torrent selection
```

Changes:

- Compose configuration, browser startup, discovery, search, normalization, and terminal presentation in the application entry point.
- Render a readable selection list containing title, seeders, leechers, category, source, and human-readable size.
- Preserve interactive selection as the default and implement explicit automatic selection of the highest-ranked eligible result.
- Apply configured and CLI result limits safely and return a clear no-results error.
- Display stage-aware progress and concise provider failure summaries without dumping browser internals by default.
- Add a non-download dry-run mode that stops after printing or selecting a result, enabling safe manual validation.

Tests:

- Unit-test display formatting, Unicode truncation, result limits, tie ordering, empty results, and automatic selection.
- Test orchestration through mock discovery and search implementations.
- Assert that dry-run never invokes a downloader.

Verification: run the standard checks and perform a local-fixture CLI dry run.

## Commit 8 — invoke local torrent clients safely

Commit message:

```text
feat(download): invoke supported local torrent clients
```

Changes:

- Add separate adapters for `aria2c`, `transmission-cli`, and compatible foreground `qbittorrent-nox` behavior.
- Detect clients in the documented priority order while allowing an explicit configuration or CLI selection.
- Verify actual client behavior and flags against the installed versions or their authoritative documentation; do not claim qBittorrent support if it cannot meet the foreground completion and seeding contract.
- Create and validate the output directory before launch.
- Pass every value as a direct process argument without shell interpolation.
- Configure clients not to continue seeding after download completion.
- Wait for the foreground client, handle interruption, propagate nonzero exits, and report completed files only when attribution is reliable.
- Never open downloaded files automatically.

Tests:

- Put fake client executables on an isolated test `PATH` and assert exact argument arrays.
- Cover client priority, explicit selection, missing clients, invalid output paths, nonzero exits, interruption, and magnets containing shell metacharacters.
- Prove that no shell process is used.

Verification: run the standard checks. Any manual real-client smoke test must use a small, clearly lawful test torrent and requires explicit operator action; it is not part of automated validation.

## Commit 9 — harden workflow limits and diagnostics

Commit message:

```text
fix(security): harden browser and download boundaries
```

Changes:

- Audit every URL transition, redirect, DOM-derived value, magnet, filesystem path, and process argument against the boundaries in `README.md`.
- Add missing global time budgets, candidate caps, redirect caps, and extraction-size limits.
- Ensure error messages identify the failing stage while redacting configuration secrets and avoiding full hostile page content.
- Ensure cancellation terminates owned browser and downloader children and removes temporary browser profiles.
- Make malformed caches disposable while malformed user configuration remains untouched.
- Document any boundary that depends on Chromium or torrent-client behavior and cannot be enforced directly by `pbtdl`.

Tests:

- Add regression tests for redirect chains, local-network destinations, hostile titles, oversized pages, path edge cases, cancellation, and resource cleanup.
- Confirm that every failure path leaves no owned child process running.

Verification: run the full standard suite and manually inspect every process-spawn and navigation call site.

## Commit 10 — complete end-to-end coverage

Commit message:

```text
test: cover the complete local workflow
```

Changes:

- Build a local integration harness containing discovery pages, failed candidates, a working Pirate Bay-shaped search page, and deterministic results.
- Exercise configuration creation, browser discovery, fallback, search, selection, dry-run, and fake-downloader execution as one workflow.
- Add opt-in live discovery diagnostics that never select or download a torrent and are excluded from normal CI.
- Add CI configuration for formatting, warnings-as-errors Clippy, and all tests on the supported Rust toolchain.
- Remove obsolete scaffolding and ensure public documentation still describes actual behavior at a high level.

Verification:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo run -- --help
git status --short
git status --short --ignored
git diff --cached --name-only
git log --oneline --decorate -10
```

The final tracked tree must contain no build output, browser profile, runtime cache, downloaded content, coverage output, log, or live-site response capture. Local ignored build output may remain on disk and must appear as ignored rather than staged or tracked.

## Completion criteria

The build is complete when:

- A first run creates a safe default XDG configuration without overwriting user data.
- The application launches an isolated system Chromium instance.
- It discovers candidates only through configured rendered pages and direct seed URLs.
- It skips blocked or unsupported candidates and finds a usable fallback.
- It searches and extracts normalized results without any torrent index API.
- Interactive and explicit automatic selection behave deterministically.
- A validated magnet is passed safely to a supported local client.
- Automated tests use only local pages, fixtures, and fake clients.
- Formatting, Clippy with warnings denied, and all tests pass.
- `target/` and every other generated build or runtime artifact are ignored and absent from both the Git index and `git ls-files`.
- The branch history uses the exact semantic commit messages specified above.
