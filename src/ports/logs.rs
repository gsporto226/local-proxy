use std::io;
use std::path::PathBuf;

/// The proxy's own log output.
pub trait LogSource: Send + Sync {
    /// The last `lines` lines of the log.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the log cannot be read; `NotFound` means no
    /// proxy has run yet.
    fn tail(&self, lines: usize) -> io::Result<String>;

    /// Where the log lives, for messages.
    fn location(&self) -> PathBuf;
}
