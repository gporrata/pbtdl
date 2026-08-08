//! Direct, foreground local torrent-client invocation with no shell interpolation.

use crate::config::DownloaderPreference;
use crate::model::MagnetUri;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use thiserror::Error;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

const CLIENT_PRIORITY: &[ClientKind] = &[
    ClientKind::Aria2c,
    ClientKind::TransmissionCli,
    ClientKind::QbittorrentNox,
];
const MAX_SNAPSHOT_ENTRIES: usize = 10_000;
const MAX_SNAPSHOT_DEPTH: usize = 32;
const CLIENT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientKind {
    Aria2c,
    TransmissionCli,
    QbittorrentNox,
}

impl ClientKind {
    pub fn executable_name(self) -> &'static str {
        match self {
            Self::Aria2c => "aria2c",
            Self::TransmissionCli => "transmission-cli",
            Self::QbittorrentNox => "qbittorrent-nox",
        }
    }

    fn contract_status(self) -> ContractStatus {
        match self {
            Self::Aria2c => ContractStatus::Supported,
            Self::TransmissionCli => ContractStatus::Unsupported(
                "transmission-cli has no supported stop-seeding-on-completion option",
            ),
            Self::QbittorrentNox => ContractStatus::Unsupported(
                "qbittorrent-nox is a long-running service and has no foreground per-download completion mode",
            ),
        }
    }
}

impl std::fmt::Display for ClientKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.executable_name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContractStatus {
    Supported,
    Unsupported(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedFile {
    pub relative_path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadOutcome {
    pub client: ClientKind,
    pub output_directory: PathBuf,
    pub new_files: Vec<DownloadedFile>,
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("no supported foreground torrent client was found; install aria2c")]
    MissingClient,
    #[error("configured torrent client was not found on PATH: {0}")]
    ConfiguredClientMissing(ClientKind),
    #[error("torrent client {client} cannot meet pbtdl's foreground/no-seeding contract: {reason}")]
    UnsupportedClient {
        client: ClientKind,
        reason: &'static str,
    },
    #[error("output path is not a usable directory: {0}")]
    InvalidOutput(PathBuf),
    #[error("output directory is too broad for safe file attribution: {0}")]
    UnsafeOutput(PathBuf),
    #[error("cannot prepare output directory {path}: {message}")]
    OutputPreparation { path: PathBuf, message: String },
    #[error("failed to start {client}: {message}")]
    Spawn { client: ClientKind, message: String },
    #[error("{client} exited unsuccessfully: {status}")]
    NonzeroExit { client: ClientKind, status: String },
    #[error("download was interrupted")]
    Interrupted,
    #[error("cannot safely inspect output directory: {0}")]
    OutputInspection(&'static str),
}

pub type DownloadResult<T> = Result<T, DownloadError>;

#[derive(Debug, Clone)]
pub struct LocalDownloader {
    preference: DownloaderPreference,
    path_entries: Vec<PathBuf>,
}

impl LocalDownloader {
    pub fn from_current_path(preference: DownloaderPreference) -> Self {
        let path_entries = std::env::var_os("PATH")
            .map(|value| std::env::split_paths(&value).collect())
            .unwrap_or_default();
        Self {
            preference,
            path_entries,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_path_entries(
        preference: DownloaderPreference,
        path_entries: Vec<PathBuf>,
    ) -> Self {
        Self {
            preference,
            path_entries,
        }
    }

    pub async fn download(
        &self,
        magnet: &MagnetUri,
        output_directory: &Path,
    ) -> DownloadResult<DownloadOutcome> {
        self.download_with_cancel(magnet, output_directory, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
    }

    pub async fn download_with_cancel(
        &self,
        magnet: &MagnetUri,
        output_directory: &Path,
        cancellation: impl Future<Output = ()> + Send,
    ) -> DownloadResult<DownloadOutcome> {
        let output_directory = prepare_output_directory(output_directory)?;
        let selected = self.select_client()?;
        let before = snapshot_files(&output_directory)?;
        let args = build_arguments(selected.kind, magnet, &output_directory)?;
        let mut command = Command::new(&selected.executable);
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| DownloadError::Spawn {
            client: selected.kind,
            message: error.to_string(),
        })?;
        tokio::pin!(cancellation);
        let status = tokio::select! {
            status = child.wait() => status.map_err(|error| DownloadError::Spawn {
                client: selected.kind,
                message: error.to_string(),
            })?,
            () = &mut cancellation => {
                child.start_kill().map_err(|error| DownloadError::Spawn {
                    client: selected.kind,
                    message: format!("failed to terminate interrupted client: {error}"),
                })?;
                timeout(CLIENT_CLEANUP_TIMEOUT, child.wait())
                    .await
                    .map_err(|_| DownloadError::Spawn {
                        client: selected.kind,
                        message: "timed out while reaping interrupted client".to_string(),
                    })?
                    .map_err(|error| DownloadError::Spawn {
                        client: selected.kind,
                        message: format!("failed to reap interrupted client: {error}"),
                    })?;
                return Err(DownloadError::Interrupted);
            }
        };
        if !status.success() {
            return Err(DownloadError::NonzeroExit {
                client: selected.kind,
                status: status.to_string(),
            });
        }
        let after = snapshot_files(&output_directory)?;
        let mut new_files: Vec<_> = after
            .into_iter()
            .filter(|(path, _)| !before.contains_key(path))
            .map(|(relative_path, size_bytes)| DownloadedFile {
                relative_path,
                size_bytes,
            })
            .collect();
        new_files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(DownloadOutcome {
            client: selected.kind,
            output_directory,
            new_files,
        })
    }

    fn select_client(&self) -> DownloadResult<SelectedClient> {
        let available = available_clients(&self.path_entries);
        let explicit = match self.preference {
            DownloaderPreference::Auto => None,
            DownloaderPreference::Aria2c => Some(ClientKind::Aria2c),
            DownloaderPreference::TransmissionCli => Some(ClientKind::TransmissionCli),
            DownloaderPreference::QbittorrentNox => Some(ClientKind::QbittorrentNox),
        };
        if let Some(kind) = explicit {
            let executable = available
                .get(&kind)
                .cloned()
                .ok_or(DownloadError::ConfiguredClientMissing(kind))?;
            ensure_supported(kind)?;
            return Ok(SelectedClient { kind, executable });
        }
        for kind in CLIENT_PRIORITY {
            if let Some(executable) = available.get(kind) {
                if matches!(kind.contract_status(), ContractStatus::Supported) {
                    return Ok(SelectedClient {
                        kind: *kind,
                        executable: executable.clone(),
                    });
                }
            }
        }
        Err(DownloadError::MissingClient)
    }
}

#[derive(Debug)]
struct SelectedClient {
    kind: ClientKind,
    executable: PathBuf,
}

fn ensure_supported(kind: ClientKind) -> DownloadResult<()> {
    match kind.contract_status() {
        ContractStatus::Supported => Ok(()),
        ContractStatus::Unsupported(reason) => Err(DownloadError::UnsupportedClient {
            client: kind,
            reason,
        }),
    }
}

fn build_arguments(
    kind: ClientKind,
    magnet: &MagnetUri,
    output_directory: &Path,
) -> DownloadResult<Vec<OsString>> {
    ensure_supported(kind)?;
    match kind {
        ClientKind::Aria2c => Ok(vec![
            OsString::from("--seed-time=0"),
            OsString::from("--dir"),
            output_directory.as_os_str().to_os_string(),
            OsString::from(magnet.as_str()),
        ]),
        ClientKind::TransmissionCli | ClientKind::QbittorrentNox => unreachable!(),
    }
}

fn prepare_output_directory(path: &Path) -> DownloadResult<PathBuf> {
    if path.exists() && !path.is_dir() {
        return Err(DownloadError::InvalidOutput(path.to_path_buf()));
    }
    if !path.exists() {
        std::fs::create_dir_all(path).map_err(|error| DownloadError::OutputPreparation {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| DownloadError::OutputPreparation {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
    if canonical.parent().is_none() {
        return Err(DownloadError::UnsafeOutput(canonical));
    }
    Ok(canonical)
}

fn available_clients(path_entries: &[PathBuf]) -> HashMap<ClientKind, PathBuf> {
    CLIENT_PRIORITY
        .iter()
        .filter_map(|kind| {
            find_on_path(OsStr::new(kind.executable_name()), path_entries).map(|path| (*kind, path))
        })
        .collect()
}

fn find_on_path(name: &OsStr, path_entries: &[PathBuf]) -> Option<PathBuf> {
    path_entries
        .iter()
        .map(|entry| entry.join(name))
        .find(|path| is_executable(path))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn snapshot_files(root: &Path) -> DownloadResult<HashMap<PathBuf, u64>> {
    let mut files = HashMap::new();
    collect_files(root, root, 0, &mut files, &mut HashSet::new())?;
    Ok(files)
}

fn collect_files(
    root: &Path,
    directory: &Path,
    depth: usize,
    files: &mut HashMap<PathBuf, u64>,
    visited: &mut HashSet<PathBuf>,
) -> DownloadResult<()> {
    if depth > MAX_SNAPSHOT_DEPTH {
        return Err(DownloadError::OutputInspection(
            "directory nesting exceeds the attribution limit",
        ));
    }
    let Ok(canonical) = directory.canonicalize() else {
        return Ok(());
    };
    if !visited.insert(canonical) {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_files(root, &path, depth + 1, files, visited)?;
        } else if file_type.is_file() {
            if let (Ok(relative), Ok(metadata)) = (path.strip_prefix(root), entry.metadata()) {
                if files.len() >= MAX_SNAPSHOT_ENTRIES {
                    return Err(DownloadError::OutputInspection(
                        "file count exceeds the attribution limit",
                    ));
                }
                files.insert(relative.to_path_buf(), metadata.len());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::str::FromStr;
    use tokio::time::Duration;

    const HASH: &str = "0123456789abcdef0123456789abcdef01234567";

    fn magnet(suffix: &str) -> MagnetUri {
        MagnetUri::from_str(&format!("magnet:?xt=urn:btih:{HASH}&dn={suffix}"))
            .expect("valid magnet")
    }

    fn write_fake(directory: &Path, name: &str, body: &str) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("write fake client");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("make executable");
        path
    }

    #[test]
    fn automatic_detection_prefers_supported_aria2c() {
        let temp = tempfile::tempdir().expect("temporary directory");
        for name in ["qbittorrent-nox", "transmission-cli", "aria2c"] {
            write_fake(temp.path(), name, "exit 0");
        }
        let downloader = LocalDownloader::with_path_entries(
            DownloaderPreference::Auto,
            vec![temp.path().to_path_buf()],
        );

        let selected = downloader.select_client().expect("select client");

        assert_eq!(selected.kind, ClientKind::Aria2c);
    }

    #[test]
    fn explicit_incompatible_clients_are_not_advertised_as_supported() {
        let temp = tempfile::tempdir().expect("temporary directory");
        write_fake(temp.path(), "transmission-cli", "exit 0");
        write_fake(temp.path(), "qbittorrent-nox", "exit 0");
        for (preference, expected) in [
            (
                DownloaderPreference::TransmissionCli,
                ClientKind::TransmissionCli,
            ),
            (
                DownloaderPreference::QbittorrentNox,
                ClientKind::QbittorrentNox,
            ),
        ] {
            let downloader =
                LocalDownloader::with_path_entries(preference, vec![temp.path().to_path_buf()]);
            assert!(matches!(
                downloader.select_client(),
                Err(DownloadError::UnsupportedClient { client, .. }) if client == expected
            ));
        }
    }

    #[test]
    fn missing_clients_and_missing_explicit_choice_are_distinct() {
        let auto = LocalDownloader::with_path_entries(DownloaderPreference::Auto, Vec::new());
        let explicit = LocalDownloader::with_path_entries(DownloaderPreference::Aria2c, Vec::new());

        assert!(matches!(
            auto.select_client(),
            Err(DownloadError::MissingClient)
        ));
        assert!(matches!(
            explicit.select_client(),
            Err(DownloadError::ConfiguredClientMissing(ClientKind::Aria2c))
        ));
    }

    #[tokio::test]
    async fn invokes_aria2c_with_exact_arguments_and_reports_new_files() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let bin = temp.path().join("bin");
        let output = temp.path().join("output");
        fs::create_dir(&bin).expect("create bin");
        let log = temp.path().join("args.log");
        let script = format!(
            r#"printf '%s\n' "$@" > '{}'
output=''
while [ "$#" -gt 0 ]; do
  if [ "$1" = '--dir' ]; then output="$2"; shift 2; else shift; fi
done
printf 'done' > "$output/completed.bin""#,
            log.display()
        );
        write_fake(&bin, "aria2c", &script);
        let downloader =
            LocalDownloader::with_path_entries(DownloaderPreference::Auto, vec![bin.clone()]);
        let magnet = magnet("legal-test");

        let outcome = downloader
            .download(&magnet, &output)
            .await
            .expect("fake download");

        let canonical_output = output.canonicalize().expect("canonical output");
        let arguments: Vec<_> = fs::read_to_string(log)
            .expect("read args")
            .lines()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            arguments,
            [
                "--seed-time=0".to_string(),
                "--dir".to_string(),
                canonical_output.display().to_string(),
                magnet.as_str().to_string(),
            ]
        );
        assert_eq!(outcome.client, ClientKind::Aria2c);
        assert_eq!(
            outcome.new_files,
            [DownloadedFile {
                relative_path: PathBuf::from("completed.bin"),
                size_bytes: 4,
            }]
        );
    }

    #[tokio::test]
    async fn magnet_shell_metacharacters_remain_one_inert_argument() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let bin = temp.path().join("bin");
        let output = temp.path().join("output");
        fs::create_dir(&bin).expect("create bin");
        let log = temp.path().join("args.log");
        let sentinel = temp.path().join("shell-was-used");
        let script = format!("printf '%s\\n' \"$@\" > '{}'", log.display());
        write_fake(&bin, "aria2c", &script);
        let downloader =
            LocalDownloader::with_path_entries(DownloaderPreference::Aria2c, vec![bin]);
        let magnet = magnet(&format!("legal;touch${{IFS}}{}", sentinel.display()));

        downloader
            .download(&magnet, &output)
            .await
            .expect("safe fake invocation");

        let arguments = fs::read_to_string(log).expect("read args");
        assert_eq!(arguments.lines().last(), Some(magnet.as_str()));
        assert!(!sentinel.exists());
    }

    #[tokio::test]
    async fn invalid_output_path_and_nonzero_exit_are_propagated() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let bin = temp.path().join("bin");
        fs::create_dir(&bin).expect("create bin");
        write_fake(&bin, "aria2c", "exit 17");
        let downloader =
            LocalDownloader::with_path_entries(DownloaderPreference::Aria2c, vec![bin]);
        let output_file = temp.path().join("not-a-directory");
        fs::write(&output_file, b"file").expect("write output file");

        assert!(matches!(
            downloader.download(&magnet("legal"), &output_file).await,
            Err(DownloadError::InvalidOutput(_))
        ));
        assert!(matches!(
            downloader
                .download(&magnet("legal"), &temp.path().join("output"))
                .await,
            Err(DownloadError::NonzeroExit { .. })
        ));
    }

    #[test]
    fn rejects_filesystem_root_and_bounded_snapshot_overflow() {
        assert!(matches!(
            prepare_output_directory(Path::new("/")),
            Err(DownloadError::UnsafeOutput(_))
        ));

        let temp = tempfile::tempdir().expect("temporary directory");
        let mut nested = temp.path().to_path_buf();
        for _ in 0..=MAX_SNAPSHOT_DEPTH {
            nested.push("nested");
            fs::create_dir(&nested).expect("create nested directory");
        }
        assert!(matches!(
            snapshot_files(temp.path()),
            Err(DownloadError::OutputInspection(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn snapshots_do_not_follow_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temporary directory");
        let outside = tempfile::tempdir().expect("outside directory");
        fs::write(outside.path().join("outside.bin"), b"outside").expect("outside file");
        symlink(outside.path(), temp.path().join("linked")).expect("create symlink");

        assert!(snapshot_files(temp.path()).expect("snapshot").is_empty());
    }

    #[tokio::test]
    async fn cancellation_kills_and_reaps_the_owned_client() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let bin = temp.path().join("bin");
        let output = temp.path().join("output");
        fs::create_dir(&bin).expect("create bin");
        let pid_file = temp.path().join("pid");
        let script = format!(
            "printf '%s' \"$$\" > '{}'\nexec sleep 60",
            pid_file.display()
        );
        write_fake(&bin, "aria2c", &script);
        let downloader =
            LocalDownloader::with_path_entries(DownloaderPreference::Aria2c, vec![bin]);

        let result = downloader
            .download_with_cancel(&magnet("legal"), &output, async {
                tokio::time::sleep(Duration::from_millis(100)).await;
            })
            .await;

        assert!(matches!(result, Err(DownloadError::Interrupted)));
        let pid = fs::read_to_string(pid_file).expect("read pid");
        assert!(!Path::new("/proc").join(pid).exists());
    }
}
