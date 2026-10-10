//! [`CommandRunner`] over child processes.

use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::io::AsyncReadExt;

use crate::domain::exec::ExecOutput;
use crate::ports::CommandRunner;

/// Runs `$proxy` commands as child processes (never through a shell).
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run<'a>(
        &'a self,
        command: &'a str,
        args: &'a [String],
        timeout: Duration,
    ) -> BoxFuture<'a, ExecOutput> {
        Box::pin(run(command, args, timeout))
    }
}

/// Run `command` with `args`, capturing stdout/stderr and enforcing `timeout`
/// (killing the child on expiry). `stdin` is closed so interactive prompts
/// cannot hang the proxy. Failures (spawn, timeout, non-zero exit) are
/// reported through [`ExecOutput`] fields.
async fn run(command: &str, args: &[String], timeout: Duration) -> ExecOutput {
    let mut child = match tokio::process::Command::new(command)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return ExecOutput {
                stdout: String::new(),
                stderr: format!("failed to spawn '{command}': {e}"),
                code: 127,
                timed_out: false,
            };
        }
    };

    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();

    let wait = async {
        let status = child.wait().await;
        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut pipe) = out_pipe.take() {
            let _ = pipe.read_to_string(&mut stdout).await;
        }
        if let Some(mut pipe) = err_pipe.take() {
            let _ = pipe.read_to_string(&mut stderr).await;
        }
        (status, stdout, stderr)
    };

    if let Ok((status, stdout, stderr)) = tokio::time::timeout(timeout, wait).await {
        ExecOutput {
            stdout,
            stderr,
            code: status.map_or(1, |s| s.code().unwrap_or(1)),
            timed_out: false,
        }
    } else {
        let _ = child.kill().await;
        let _ = child.wait().await;
        ExecOutput {
            stdout: String::new(),
            stderr: format!("command timed out after {timeout:?}"),
            code: 124,
            timed_out: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_binary_reports_spawn_failure() {
        let out = ProcessCommandRunner
            .run("local-proxy-no-such-binary", &[], Duration::from_secs(5))
            .await;
        assert_eq!(out.code, 127);
        assert!(out.stderr.contains("failed to spawn"));
    }
}
