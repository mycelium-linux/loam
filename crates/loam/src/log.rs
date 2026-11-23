//! Small size-based log files for the daemon.
//!
//! `loamd` already keeps the last run in memory, which is the right answer for
//! `inspect` and the wrong answer after a restart. This module is the durable
//! half: one file per instance, a bounded size, and enough rotation that a
//! command which logs every frame cannot fill the disk.
//!
//! It is deliberately not a logging framework. There is one writer, records are
//! appended whole, and the newest file is the one with no numeric suffix:
//!
//! ```text
//! logs/build.log      current
//! logs/build.log.1    previous
//! logs/build.log.2    oldest kept
//! ```

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Rotate once a file would grow past one mebibyte.
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;

/// Keep the current file plus four previous generations.
pub const DEFAULT_KEEP: usize = 4;

/// Where a daemon that was not given a directory puts its logs.
pub fn default_root() -> PathBuf {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(state).join("loam/logs");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".local/state/loam/logs")
}

/// A directory of size-rotated log files.
#[derive(Clone)]
pub struct LogStore {
    root: PathBuf,
    max_bytes: u64,
    keep: usize,
    lock: Arc<Mutex<()>>,
}

impl LogStore {
    pub fn new(root: impl Into<PathBuf>, max_bytes: u64, keep: usize) -> io::Result<Self> {
        if max_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log max bytes must be greater than zero",
            ));
        }
        if keep == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log keep must be greater than zero",
            ));
        }
        Ok(Self {
            root: root.into(),
            max_bytes,
            keep,
            lock: Arc::new(Mutex::new(())),
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file an instance id writes to. Ids are paths from an API, so this
    /// reduces them to one safe component rather than trusting them.
    #[must_use]
    pub fn path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{}.log", file_stem(id)))
    }

    /// Append one record, rotating before the write when it would exceed the
    /// limit. The returned path is the current file, not a rotated one.
    pub fn append(&self, id: &str, bytes: &[u8]) -> io::Result<PathBuf> {
        if bytes.is_empty() {
            return Ok(self.path(id));
        }
        let _guard = self.lock.lock().expect("log lock poisoned");
        fs::create_dir_all(&self.root)?;
        let path = self.path(id);
        if fs::metadata(&path).is_ok_and(|metadata| {
            metadata.len() > 0 && metadata.len().saturating_add(bytes.len() as u64) > self.max_bytes
        }) {
            self.rotate(&path)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        file.write_all(bytes)?;
        file.flush()?;
        Ok(path)
    }

    /// Read the current log, when one exists.
    pub fn read(&self, id: &str) -> io::Result<Option<String>> {
        let path = self.path(id);
        match fs::read_to_string(path) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn rotate(&self, path: &Path) -> io::Result<()> {
        for index in (1..self.keep).rev() {
            let from = rotated(path, index);
            let to = rotated(path, index + 1);
            if from.is_file() {
                let _ = fs::remove_file(&to);
                fs::rename(from, to)?;
            }
        }
        let first = rotated(path, 1);
        let _ = fs::remove_file(&first);
        fs::rename(path, first)
    }
}

fn rotated(path: &Path, index: usize) -> PathBuf {
    PathBuf::from(format!("{}.{index}", path.display()))
}

/// One filesystem-safe component. Empty or all-punctuation ids still get a
/// name, so a malformed request cannot make every instance share a file.
fn file_stem(id: &str) -> String {
    let mut stem = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if stem.is_empty() || stem.chars().all(|ch| ch == '_' || ch == '.') {
        stem = "instance".into();
    }
    stem
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_reads_one_file() {
        let temp = tempfile::tempdir().unwrap();
        let logs = LogStore::new(temp.path(), 1024, 2).unwrap();
        logs.append("build", b"first\n").unwrap();
        logs.append("build", b"second\n").unwrap();
        assert_eq!(
            logs.read("build").unwrap().as_deref(),
            Some("first\nsecond\n")
        );
        assert_eq!(logs.path("build"), temp.path().join("build.log"));
    }

    #[test]
    fn rotates_before_crossing_the_limit_and_keeps_generations() {
        let temp = tempfile::tempdir().unwrap();
        let logs = LogStore::new(temp.path(), 8, 2).unwrap();
        logs.append("run", b"12345678").unwrap();
        logs.append("run", b"abcdefgh").unwrap();
        logs.append("run", b"ijklmnop").unwrap();
        assert_eq!(logs.read("run").unwrap().as_deref(), Some("ijklmnop"));
        assert_eq!(
            fs::read_to_string(temp.path().join("run.log.1")).unwrap(),
            "abcdefgh"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("run.log.2")).unwrap(),
            "12345678"
        );
        assert!(!temp.path().join("run.log.3").exists());
    }

    #[test]
    fn ids_cannot_escape_the_log_directory() {
        let temp = tempfile::tempdir().unwrap();
        let logs = LogStore::new(temp.path(), 1024, 2).unwrap();
        assert_eq!(
            logs.path("../../etc/passwd"),
            temp.path().join(".._.._etc_passwd.log")
        );
        assert_eq!(logs.path(""), temp.path().join("instance.log"));
        assert_eq!(logs.path("..."), temp.path().join("instance.log"));
    }

    #[test]
    fn refuses_a_zero_limit_or_zero_keep() {
        let temp = tempfile::tempdir().unwrap();
        assert!(LogStore::new(temp.path(), 0, 2).is_err());
        assert!(LogStore::new(temp.path(), 1, 0).is_err());
    }
}
