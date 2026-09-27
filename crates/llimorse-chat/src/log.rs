//! [`llimorse::ChatListener`] implementation to keep a log.

use anyhow::{Result, anyhow};
use chrono::Local;
use llimorse::ChatListener;
use llimorse::line_format::ChatMessage;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;
use tracing::error;

#[cfg(feature = "clap")]
use clap::builder::{TypedValueParser, ValueParserFactory};
#[cfg(feature = "clap")]
use clap::error::Result as ClapResult;
#[cfg(feature = "clap")]
use clap::{Arg, Command};

/// [`llimorse::ChatListener`] implementation to keep a log in a file.
#[derive(Debug)]
pub struct SessionLog {
    /// Where to write the log output
    output: Option<File>,
}

impl SessionLog {
    /// Store the log in the given file.
    pub fn new<P: AsRef<Path>>(file: P) -> Result<Self> {
        Ok(SessionLog {
            output: Some(
                OpenOptions::new()
                    .create_new(true)
                    .append(true)
                    .open(file)?,
            ),
        })
    }

    /// Load the log from `load_from`.
    ///
    /// This log can be applied via [`llimorse::Agent::push_history()`].
    pub fn load<P: AsRef<Path>>(load_from: P) -> Result<Vec<ChatMessage>> {
        BufReader::new(File::open(load_from)?)
            .lines()
            .map(|line| -> Result<ChatMessage> {
                let line = line.map_err(|err| anyhow!("Failed to read data: {err}"))?;
                serde_json::from_str(&line)
                    .map_err(|err| anyhow!("Failed to parse data: {line}: {err}"))
            })
            .collect()
    }

    /// Create a null log (i.e. does not store anything).
    ///
    /// This helps creating an `Agent<SessionLog>` that can both store a log or not.
    pub fn null() -> Self {
        SessionLog { output: None }
    }

    /// Log the given `message` to the output, raising errors.
    fn do_log(&mut self, message: &llimorse::line_format::ChatMessage) -> Result<()> {
        let Some(file) = &mut self.output else {
            return Ok(());
        };

        let json = serde_json::to_string(message)
            .map_err(|err| anyhow!("Failed to serialize {message:?}: {err}"))?;

        file.write_all(json.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;

        Ok(())
    }
}

impl ChatListener for SessionLog {
    fn log_message(&mut self, message: &ChatMessage) {
        if let Err(err) = self.do_log(message) {
            error!("Failed to log {message:?}: {err}; log will be discontinued from here");
            self.output.take();
        }
    }
}

/// What a caller asked for with its `--resume` flag (or equivalent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resume {
    /// No resume requested: start a fresh session.
    Fresh,
    /// Resume the newest non-empty session log in the session-logs directory.
    Newest,
    /// Resume the given session log file.
    File(PathBuf),
}

#[cfg(feature = "clap")]
impl Resume {
    /// Create a clap value parser for Resume that handles optional file paths.
    ///
    /// This parser supports:
    /// - `--resume` with no value -> `Resume::Newest`
    /// - `--resume <path>` -> `Resume::File(path)`
    pub fn value_parser() -> ResumeValueParser {
        ResumeValueParser
    }
}

#[cfg(feature = "clap")]
/// Value parser for [`Resume`] that handles optional file paths in clap arguments.
#[derive(Clone)]
pub struct ResumeValueParser;

#[cfg(feature = "clap")]
impl TypedValueParser for ResumeValueParser {
    type Value = Resume;

    fn parse_ref(
        &self,
        _cmd: &Command,
        _arg: Option<&Arg>,
        value: &std::ffi::OsStr,
    ) -> ClapResult<Self::Value> {
        if value.is_empty() {
            Ok(Resume::Newest)
        } else {
            Ok(Resume::File(PathBuf::from(value)))
        }
    }
}

#[cfg(feature = "clap")]
impl ValueParserFactory for Resume {
    type Parser = ResumeValueParser;

    fn value_parser() -> Self::Parser {
        ResumeValueParser
    }
}

/// Manages the storage and retrieval of session logs.
///
/// Creates the log file that the current run writes to, and loads the transcript of the session
/// being resumed, if any.
#[derive(Debug)]
pub struct SessionManager {
    /// The session log this run writes to (null when no session-logs directory was given).
    pub log: SessionLog,
    /// Transcript of the session being resumed; empty when starting fresh.
    pub history: Vec<ChatMessage>,
}

impl SessionManager {
    /// Create the session log for this run and, if requested, load the transcript of the session
    /// to resume.
    ///
    /// The resume target is resolved *before* the new log file is created, so the file created by
    /// this call can never be picked as the newest log.
    ///
    /// A `Resume::File` whose path is a bare file name (no directory part) and not found in the
    /// current directory is looked up in the session-logs directory, if one was given, also
    /// trying `<name>.jsonl` when the name has no extension.
    pub fn new(session_logs: Option<&Path>, resume: &Resume) -> Result<Self> {
        let history = match resume {
            Resume::Fresh => Vec::new(),
            Resume::Newest => {
                let dir = session_logs.ok_or_else(|| {
                    anyhow!(
                        "No session log to resume: picking the newest log \
                         requires a session-logs directory, but none was given"
                    )
                })?;
                let file = Self::newest_log(dir)?;
                SessionLog::load(&file).map_err(|err| anyhow!("{}: {err}", file.display()))?
            }
            Resume::File(file) => {
                let file = Self::resolve_resume_file(file, session_logs)?;
                SessionLog::load(&file).map_err(|err| anyhow!("{}: {err}", file.display()))?
            }
        };

        let log = match session_logs {
            Some(dir) => {
                let path = dir.join(Self::new_log_name());
                SessionLog::new(&path).map_err(|err| anyhow!("{}: {err}", path.display()))?
            }
            None => SessionLog::null(),
        };

        Ok(SessionManager { log, history })
    }

    /// Find the newest non-empty session log in `dir`, by modification time (ties broken by name,
    /// which is timestamped).
    fn newest_log(dir: &Path) -> Result<PathBuf> {
        let mut newest: Option<(SystemTime, PathBuf)> = None;
        for entry in fs::read_dir(dir).map_err(|err| anyhow!("{}: {err}", dir.display()))? {
            let entry = entry.map_err(|err| anyhow!("{}: {err}", dir.display()))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let meta = entry
                .metadata()
                .map_err(|err| anyhow!("{}: {err}", path.display()))?;
            if !meta.is_file() || meta.len() == 0 {
                continue;
            }
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let supersedes = match &newest {
                None => true,
                Some((time, name)) => mtime > *time || (mtime == *time && path > *name),
            };
            if supersedes {
                newest = Some((mtime, path));
            }
        }
        newest.map(|(_, path)| path).ok_or_else(|| {
            anyhow!(
                "No non-empty session log found in {} to resume",
                dir.display()
            )
        })
    }

    /// Resolve the file to resume from.
    ///
    /// A bare file name (no directory part, i.e. no slash) that does not exist in the current
    /// directory is looked up in the session-logs directory, if one was given, also trying
    /// `<name>.jsonl` when the name has no extension. Anything else is used as-is: a path that
    /// exists nowhere then fails with the natural load error, unless a directory was given and
    /// the bare name is in neither place, which is its own error.
    fn resolve_resume_file(file: &Path, session_logs: Option<&Path>) -> Result<PathBuf> {
        if file.exists() || !Self::is_bare_name(file) {
            return Ok(file.to_path_buf());
        }
        let Some(dir) = session_logs else {
            return Ok(file.to_path_buf());
        };
        let mut candidates = vec![dir.join(file)];
        if !file.to_string_lossy().ends_with(".jsonl") {
            candidates.push(dir.join(format!("{}.jsonl", file.display())));
        }
        for candidate in &candidates {
            if candidate.exists() {
                return Ok(candidate.clone());
            }
        }
        let mut names = vec![file.display().to_string()];
        if let Some(candidate) = candidates.get(1) {
            names.push(candidate.display().to_string());
        }
        Err(anyhow!(
            "Session log {} not found (looked in the current directory and in {})",
            names.join(" or "),
            dir.display()
        ))
    }

    /// A bare file name: exactly one path component, so no directory part (no slash).
    fn is_bare_name(file: &Path) -> bool {
        let mut components = file.components();
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
    }

    /// A session log file name, timestamped so that names sort chronologically.
    fn new_log_name() -> String {
        Local::now().format("%Y-%m-%dT%H_%M_%S.jsonl").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory for one test, removed on drop.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        /// Create a fresh scratch directory under `$TMPDIR`.
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("session-manager-{name}-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            ScratchDir(dir)
        }

        /// The underlying path.
        fn path(&self) -> &Path {
            &self.0
        }

        /// Join a file name onto the scratch directory.
        fn join(&self, file: &str) -> PathBuf {
            self.0.join(file)
        }

        /// Write `content` to `file` inside the scratch directory.
        fn write(&self, file: &str, content: &str) {
            fs::write(self.join(file), content).unwrap();
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// File selection: `.jsonl` only, non-empty, regular files — by mtime.
    #[test]
    fn newest_log_picks_the_jsonl_file_and_skips_empty_and_foreign_files() {
        let dir = ScratchDir::new("pick");
        dir.write("2026-01-01T00_00_00.jsonl", "not-empty\n");
        dir.write("2026-01-02T00_00_00.jsonl", "");
        dir.write("notes.txt", "not a session log\n");
        fs::create_dir(dir.join("2026-01-03T00_00_00.jsonl")).unwrap();

        let newest = SessionManager::newest_log(dir.path()).unwrap();
        assert_eq!(newest, dir.join("2026-01-01T00_00_00.jsonl"));
    }

    /// A missing directory is an error, not a fresh start.
    #[test]
    fn newest_log_errors_on_a_missing_directory() {
        let dir = ScratchDir::new("missing");
        let missing = dir.join("does-not-exist");
        let err = SessionManager::newest_log(&missing).unwrap_err();
        assert!(err.to_string().contains("does-not-exist"));
    }

    /// A directory without any usable log is an error.
    #[test]
    fn newest_log_errors_when_no_log_is_usable() {
        let dir = ScratchDir::new("empty");
        dir.write("2026-01-01T00_00_00.jsonl", "");
        dir.write("notes.txt", "hello\n");

        let err = SessionManager::newest_log(dir.path()).unwrap_err();
        assert!(err.to_string().contains("No non-empty session log"));
    }

    /// With no resume requested and no directory, nothing is loaded.
    #[test]
    fn fresh_with_no_directory_creates_nothing() {
        let manager = SessionManager::new(None, &Resume::Fresh).unwrap();
        assert!(manager.history.is_empty());
    }

    /// `Resume::Newest` without a directory is an error.
    #[test]
    fn newest_without_a_directory_is_an_error() {
        let err = SessionManager::new(None, &Resume::Newest).unwrap_err();
        assert!(
            err.to_string()
                .contains("requires a session-logs directory")
        );
    }

    /// A fresh session in a directory creates exactly one log file there.
    #[test]
    fn fresh_with_a_directory_creates_a_log_file() {
        let dir = ScratchDir::new("fresh");
        let manager = SessionManager::new(Some(dir.path()), &Resume::Fresh).unwrap();
        assert!(manager.history.is_empty());

        let files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(files.len(), 1);
        assert!(files[0].to_string_lossy().ends_with(".jsonl"));
    }

    /// Resuming a file loads its messages, and the fresh log is created
    /// alongside the resumed one.
    #[test]
    fn resume_a_file_loads_its_messages() {
        let dir = ScratchDir::new("resume-file");
        dir.write(
            "2026-01-01T00_00_00.jsonl",
            "{\"role\":\"user\",\"content\":\"hi\"}\n",
        );
        let old = dir.join("2026-01-01T00_00_00.jsonl");

        let manager = SessionManager::new(Some(dir.path()), &Resume::File(old)).unwrap();
        assert_eq!(manager.history.len(), 1);

        // The fresh log was created next to the resumed one.
        let files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(files.len(), 2);
    }

    /// A bare file name that is not in the current directory is looked up in the
    /// session-logs directory.
    #[test]
    fn bare_resume_name_falls_back_to_the_session_logs_dir() {
        let dir = ScratchDir::new("fallback");
        dir.write(
            "session-bare.jsonl",
            "{\"role\":\"user\",\"content\":\"hi\"}\n",
        );

        let manager = SessionManager::new(
            Some(dir.path()),
            &Resume::File(PathBuf::from("session-bare.jsonl")),
        )
        .unwrap();
        assert_eq!(manager.history.len(), 1);
    }

    /// A bare file name without an extension is looked up in the session-logs directory with
    /// `.jsonl` appended.
    #[test]
    fn bare_resume_name_without_an_extension_tries_jsonl_in_the_session_logs_dir() {
        let dir = ScratchDir::new("jsonl-fallback");
        dir.write(
            "session-bare.jsonl",
            "{\"role\":\"user\",\"content\":\"hi\"}\n",
        );

        let manager = SessionManager::new(
            Some(dir.path()),
            &Resume::File(PathBuf::from("session-bare")),
        )
        .unwrap();
        assert_eq!(manager.history.len(), 1);
    }

    /// A path with a directory part is used as-is: no lookup in the session-logs directory.
    #[test]
    fn resume_path_with_a_directory_part_is_used_as_is() {
        let dir = ScratchDir::new("no-fallback");
        fs::create_dir(dir.join("inner")).unwrap();
        fs::write(
            dir.join("inner").join("session-bare.jsonl"),
            "{\"role\":\"user\",\"content\":\"hi\"}\n",
        )
        .unwrap();

        // The file exists only under the session-logs directory, at a path that does not exist
        // in the current directory, so the load must fail rather than fall back.
        let err = SessionManager::new(
            Some(dir.path()),
            &Resume::File(PathBuf::from("inner/session-bare.jsonl")),
        )
        .unwrap_err();
        assert!(err.to_string().contains("inner/session-bare.jsonl"));
    }

    /// A bare name in neither the current directory nor the session-logs directory is an error
    /// that mentions both places.
    #[test]
    fn bare_resume_name_in_neither_place_is_an_error() {
        let dir = ScratchDir::new("nowhere");
        let err = SessionManager::new(
            Some(dir.path()),
            &Resume::File(PathBuf::from("no-such-session.jsonl")),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no-such-session.jsonl"));
        assert!(msg.contains(&*dir.path().to_string_lossy()));
    }

    /// A bare name without an extension, in neither place, is an error that names the `.jsonl`
    /// candidate that was tried in the session-logs directory as well.
    #[test]
    fn bare_resume_name_without_an_extension_in_neither_place_is_an_error() {
        let dir = ScratchDir::new("nowhere-jsonl");
        let err = SessionManager::new(
            Some(dir.path()),
            &Resume::File(PathBuf::from("no-such-session")),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no-such-session"));
        assert!(msg.contains("no-such-session.jsonl"));
        assert!(msg.contains(&*dir.path().to_string_lossy()));
    }

    /// A bare name that is not found, with no session-logs directory, keeps the natural load
    /// error.
    #[test]
    fn bare_resume_name_without_a_session_logs_dir_is_an_error() {
        let err = SessionManager::new(None, &Resume::File(PathBuf::from("no-such-session.jsonl")))
            .unwrap_err();
        assert!(err.to_string().contains("no-such-session.jsonl"));
    }

    /// `Resume::Newest` picks up the newest non-empty log in the directory.
    #[test]
    fn newest_resumes_the_newest_log() {
        let dir = ScratchDir::new("newest");
        dir.write(
            "2026-01-01T00_00_00.jsonl",
            "{\"role\":\"user\",\"content\":\"old\"}\n",
        );
        dir.write(
            "2026-01-02T00_00_00.jsonl",
            "{\"role\":\"user\",\"content\":\"new\"}\n",
        );

        let manager = SessionManager::new(Some(dir.path()), &Resume::Newest).unwrap();
        assert_eq!(manager.history.len(), 1);
    }
}
