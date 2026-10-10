//! [`LogSource`] over the proxy's log file.

use std::io;
use std::path::{Path, PathBuf};

use crate::adapters::outbound::paths;
use crate::ports::LogSource;

/// Reads `<config dir>/local-proxy.log`, resolved per call.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileLogSource;

impl LogSource for FileLogSource {
    fn tail(&self, lines: usize) -> io::Result<String> {
        tail_lines(&paths::log_file(), lines)
    }

    fn location(&self) -> PathBuf {
        paths::log_file()
    }
}

/// The last `lines` lines of a text file, joined without a trailing newline.
/// Invalid UTF-8 bytes are replaced rather than failing the read.
fn tail_lines(path: &Path, lines: usize) -> io::Result<String> {
    // ponytail: reads the whole file; the log is truncated on every start, so
    // a reverse block read is only worth it if logs ever grow huge.
    let raw = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&raw);
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    Ok(all[start..].join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_lines_returns_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local-proxy.log");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        assert_eq!(tail_lines(&path, 2).unwrap(), "two\nthree");
        assert_eq!(tail_lines(&path, 9).unwrap(), "one\ntwo\nthree");
        assert_eq!(tail_lines(&path, 0).unwrap(), "");
    }

    #[test]
    fn tail_lines_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = tail_lines(&dir.path().join("nope.log"), 5).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
