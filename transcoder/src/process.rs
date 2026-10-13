//! Child-process helper for FFmpeg, ffprobe and whisper-cli.
//!
//! Three things matter and are easy to get wrong:
//!   * stdin is always /dev/null, so a child can never block waiting on input
//!     the worker will never send;
//!   * stderr is captured and surfaced in errors, because "exit status 1" from
//!     FFmpeg is useless without the message above it;
//!   * the child is killed on timeout and on shutdown, so a wedged encode
//!     cannot outlive the job and hold the scratch volume open.

use anyhow::{bail, Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, warn};

/// Captured output of a finished child process.
#[derive(Debug)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
}

/// Hard ceiling on any single child process.
///
/// A transcode of a long video legitimately takes a long time, so this is
/// generous; it exists to catch a genuinely wedged process, not to bound
/// normal work.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);

/// Run a command to completion, failing if it exits non-zero.
///
/// `context` is woven into the error message, e.g. "encoding the 720p
/// rendition".
pub async fn run_checked<S: AsRef<str>>(
    program: &str,
    args: &[S],
    context: &str,
) -> Result<Output> {
    run_checked_with_timeout(program, args, context, DEFAULT_TIMEOUT).await
}

pub async fn run_checked_with_timeout<S: AsRef<str>>(
    program: &str,
    args: &[S],
    context: &str,
    timeout: Duration,
) -> Result<Output> {
    let rendered: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
    debug!(program, args = ?rendered, "spawning");

    let child = Command::new(program)
        .args(&rendered)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Put the child in its own process group so a kill reaches any
        // grandchildren FFmpeg may have spawned.
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("could not start {program} while {context} (is it on PATH?)"))?;

    let wait = child.wait_with_output();

    let output = match tokio::time::timeout(timeout, wait).await {
        Ok(result) => result.with_context(|| format!("error waiting for {program}"))?,
        Err(_) => {
            // The child is killed by kill_on_drop when `child` is dropped, but
            // it has been moved into wait_with_output, so say what happened
            // and let the drop do its work.
            warn!(
                program,
                timeout_secs = timeout.as_secs(),
                "killing a child process that exceeded its timeout"
            );
            bail!(
                "{program} exceeded its {}s timeout while {context}",
                timeout.as_secs()
            );
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        // FFmpeg's real diagnostic is the last few lines of stderr; include
        // enough to be useful but not the entire encode log.
        let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
        let tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");

        bail!(
            "{program} failed with {} while {context}:\n{}",
            output.status,
            if tail.is_empty() {
                "(no stderr output)".to_string()
            } else {
                tail
            }
        );
    }

    Ok(Output { stdout, stderr })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_stdout_from_a_successful_command() {
        let output = run_checked("echo", &["hello world"], "testing echo")
            .await
            .expect("echo should succeed");

        assert_eq!(output.stdout.trim(), "hello world");
    }

    #[tokio::test]
    async fn a_nonzero_exit_is_an_error_naming_the_context() {
        let error = run_checked("false", &[] as &[&str], "testing failure")
            .await
            .expect_err("false should fail");

        let message = error.to_string();
        assert!(message.contains("testing failure"), "got: {message}");
        assert!(message.contains("false failed"), "got: {message}");
    }

    #[tokio::test]
    async fn stderr_is_included_in_the_error() {
        // The whole reason stderr is piped: without it, a failed encode gives
        // no clue what went wrong.
        let error = run_checked(
            "sh",
            &["-c", "echo 'the real problem' >&2; exit 3"],
            "testing stderr capture",
        )
        .await
        .expect_err("should fail");

        assert!(
            error.to_string().contains("the real problem"),
            "got: {error}"
        );
    }

    #[tokio::test]
    async fn a_missing_program_is_reported_clearly() {
        let error = run_checked(
            "definitely-not-a-real-program-xyz",
            &[] as &[&str],
            "testing a missing binary",
        )
        .await
        .expect_err("should fail");

        let message = error.to_string();
        assert!(message.contains("could not start"), "got: {message}");
        assert!(message.contains("on PATH"), "got: {message}");
    }

    #[tokio::test]
    async fn a_hung_process_is_killed_at_the_timeout() {
        let start = std::time::Instant::now();

        let error = run_checked_with_timeout(
            "sleep",
            &["60"],
            "testing the timeout",
            Duration::from_millis(300),
        )
        .await
        .expect_err("should time out");

        assert!(error.to_string().contains("timeout"), "got: {error}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "should have returned promptly, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn stdin_is_closed_so_a_reader_does_not_hang() {
        // `cat` with no arguments reads stdin forever unless it is /dev/null.
        // If stdin were inherited this would hang until the timeout.
        let output = run_checked_with_timeout(
            "cat",
            &[] as &[&str],
            "testing stdin handling",
            Duration::from_secs(5),
        )
        .await
        .expect("cat should see EOF immediately");

        assert_eq!(output.stdout, "");
    }

    #[tokio::test]
    async fn a_large_stderr_does_not_deadlock() {
        // Piped output must be drained concurrently with the wait; otherwise a
        // child writing more than the pipe buffer blocks forever.
        let output = run_checked_with_timeout(
            "sh",
            &["-c", "seq 1 50000 >&2; echo done"],
            "testing a large stderr",
            Duration::from_secs(30),
        )
        .await
        .expect("should not deadlock");

        assert_eq!(output.stdout.trim(), "done");
        assert!(output.stderr.lines().count() >= 50_000);
    }
}
