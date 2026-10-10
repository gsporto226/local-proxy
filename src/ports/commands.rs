use std::time::Duration;

use futures_util::future::BoxFuture;

use crate::domain::exec::ExecOutput;

/// Runs local commands for the `$proxy` token.
pub trait CommandRunner: Send + Sync {
    /// Run `command` with `args` (no shell), killing it after `timeout`.
    /// Failures are reported through the [`ExecOutput`] fields.
    fn run<'a>(
        &'a self,
        command: &'a str,
        args: &'a [String],
        timeout: Duration,
    ) -> BoxFuture<'a, ExecOutput>;
}
