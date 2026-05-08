//! Fork+exec a child process and pump its stdio through bounded mpsc
//! channels. cgroup placement (`exec_in_scope`) is a thin wrapper around
//! this — it runs the same command pipeline with a pre-fork hook that
//! writes the helper PID into the per-scope `cgroup.procs`.
//!
//! All the *testable* logic lives here: command launch, stdio pipe
//! adapters, exit-status decoding. The cgroup hook is gated on
//! `target_os = "linux"` and additionally requires root + a real
//! cgroup-v2 mount at runtime, neither of which CI provides.

use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::warn;

use classic_proto::StdinKind;

/// Maximum bytes buffered per stdio direction (per plan: ~1 MiB).
pub const STDIO_CHANNEL_CAP: usize = 1024;

/// Child process exit summary. Either `code` is `Some` (normal exit) or
/// `signal` is `Some` (killed by a signal); both `None` means the
/// kernel never returned a status (extreme cases — Tokio drops, etc.).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ChildExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("spawning {argv0:?}: {source}")]
    Spawn {
        argv0: String,
        #[source]
        source: std::io::Error,
    },
    #[error("waiting on child: {0}")]
    Wait(#[from] std::io::Error),
    #[error("argv must have at least one element")]
    EmptyArgv,
}

/// Handle returned from `exec_command`. The receiver streams resolve EOF
/// when the child closes its end of the pipe; the sender's empty `Vec<u8>`
/// closes the child's stdin.
pub struct ChildHandle {
    pub req_id: u64,
    pub stdout: mpsc::Receiver<Vec<u8>>,
    pub stderr: mpsc::Receiver<Vec<u8>>,
    pub stdin: mpsc::Sender<Vec<u8>>,
    /// Future that resolves to the exit info once the child terminates.
    /// Implemented as a `JoinHandle` so callers can race wait against
    /// other futures with `tokio::select!`.
    wait: tokio::task::JoinHandle<Result<ChildExitInfo, ExecError>>,
}

impl std::fmt::Debug for ChildHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildHandle")
            .field("req_id", &self.req_id)
            .finish_non_exhaustive()
    }
}

impl ChildHandle {
    pub async fn wait(self) -> Result<ChildExitInfo, ExecError> {
        match self.wait.await {
            Ok(r) => r,
            Err(e) => Err(ExecError::Wait(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("wait task: {e}"),
            ))),
        }
    }
}

/// Launch a command with three pumps (stdout, stderr, stdin) and return
/// a `ChildHandle` for the caller to drive. `req_id` is the originating
/// spawn-request id — passed through here unchanged so the higher layers
/// can correlate ChildStdio / ChildExit frames.
pub async fn exec_command(
    req_id: u64,
    argv: &[String],
    env: &[(String, String)],
    stdin_kind: Option<StdinKind>,
) -> Result<ChildHandle, ExecError> {
    let argv0 = argv.first().cloned().ok_or(ExecError::EmptyArgv)?;
    let mut cmd = Command::new(&argv0);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    // The plan is explicit: the CLI's own env is NOT forwarded. Start
    // from an empty env and add only what the caller supplied.
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    match stdin_kind {
        Some(StdinKind::Inherit) | Some(StdinKind::File) => cmd.stdin(Stdio::piped()),
        None => cmd.stdin(Stdio::null()),
    };

    let mut child = cmd.spawn().map_err(|source| ExecError::Spawn {
        argv0: argv0.clone(),
        source,
    })?;

    let (stdout_tx, stdout_rx) = mpsc::channel(STDIO_CHANNEL_CAP);
    let (stderr_tx, stderr_rx) = mpsc::channel(STDIO_CHANNEL_CAP);
    let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>(STDIO_CHANNEL_CAP);

    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(pump_read(stdout, stdout_tx));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(pump_read(stderr, stderr_tx));
    }
    if let Some(stdin) = child.stdin.take() {
        tokio::spawn(pump_write(stdin, stdin_rx));
    }

    let wait = tokio::spawn(async move {
        let status = child.wait().await?;
        Ok(decode_status(status))
    });

    Ok(ChildHandle {
        req_id,
        stdout: stdout_rx,
        stderr: stderr_rx,
        stdin: stdin_tx,
        wait,
    })
}

fn decode_status(status: std::process::ExitStatus) -> ChildExitInfo {
    use std::os::unix::process::ExitStatusExt;
    ChildExitInfo {
        code: status.code(),
        signal: status.signal(),
    }
}

async fn pump_read<R: tokio::io::AsyncRead + Unpin>(mut reader: R, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = vec![0u8; 8 * 1024];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,                 // EOF
            Ok(n) => n,
            Err(e) => {
                warn!(error = %e, "stdio read failed; closing pump");
                break;
            }
        };
        if tx.send(buf[..n].to_vec()).await.is_err() {
            // Receiver dropped — child likely orphaned.
            break;
        }
    }
    // Send empty Vec to mark EOF for the consumer.
    let _ = tx.send(Vec::new()).await;
}

async fn pump_write<W: tokio::io::AsyncWrite + Unpin>(mut writer: W, mut rx: mpsc::Receiver<Vec<u8>>) {
    while let Some(chunk) = rx.recv().await {
        if chunk.is_empty() {
            // Empty Vec means "close stdin". Drop the writer to send EOF.
            break;
        }
        if writer.write_all(&chunk).await.is_err() {
            break;
        }
    }
    // Drop closes stdin.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn echo_yields_stdout_and_zero_exit() {
        let mut h = exec_command(1, &argv(&["/bin/echo", "hi"]), &[], None)
            .await
            .unwrap();
        // Drain stdout until EOF.
        let mut out = Vec::new();
        while let Some(chunk) = h.stdout.recv().await {
            if chunk.is_empty() {
                break;
            }
            out.extend(chunk);
        }
        let info = h.wait.await.unwrap().unwrap();
        assert_eq!(info.code, Some(0));
        assert!(info.signal.is_none());
        assert_eq!(&out, b"hi\n");
    }

    #[tokio::test]
    async fn false_yields_exit_code_one() {
        let h = exec_command(1, &argv(&["/bin/false"]), &[], None)
            .await
            .unwrap();
        let info = h.wait().await.unwrap();
        assert_eq!(info.code, Some(1));
    }

    #[tokio::test]
    async fn missing_binary_returns_spawn_error() {
        let err = exec_command(1, &argv(&["/no/such/binary/foobar"]), &[], None)
            .await
            .unwrap_err();
        match err {
            ExecError::Spawn { source, .. } => {
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected Spawn(NotFound), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_argv_rejected() {
        let err = exec_command(1, &[], &[], None).await.unwrap_err();
        assert!(matches!(err, ExecError::EmptyArgv));
    }

    #[tokio::test]
    async fn stdin_pump_round_trips_through_cat() {
        let h = exec_command(
            1,
            &argv(&["/bin/cat"]),
            &[],
            Some(StdinKind::Inherit),
        )
        .await
        .unwrap();

        // Send some bytes then close stdin (empty Vec).
        h.stdin.send(b"hello classic\n".to_vec()).await.unwrap();
        h.stdin.send(Vec::new()).await.unwrap();

        // Drain stdout until EOF.
        let stdout_rx = h.stdout;
        let mut out = Vec::new();
        let mut stdout_rx = stdout_rx;
        while let Some(chunk) = stdout_rx.recv().await {
            if chunk.is_empty() {
                break;
            }
            out.extend(chunk);
        }
        let info = h.wait.await.unwrap().unwrap();
        assert_eq!(info.code, Some(0));
        assert_eq!(&out, b"hello classic\n");
    }

    #[tokio::test]
    async fn stderr_separately_captured() {
        // /bin/sh -c 'echo err >&2' writes to stderr only.
        let h = exec_command(
            1,
            &argv(&["/bin/sh", "-c", "echo err >&2"]),
            &[],
            None,
        )
        .await
        .unwrap();
        let mut err = Vec::new();
        let mut stderr_rx = h.stderr;
        while let Some(chunk) = stderr_rx.recv().await {
            if chunk.is_empty() {
                break;
            }
            err.extend(chunk);
        }
        let info = h.wait.await.unwrap().unwrap();
        assert_eq!(info.code, Some(0));
        assert_eq!(&err, b"err\n");
    }

    #[tokio::test]
    async fn env_only_carries_what_was_passed() {
        // /bin/sh -c 'echo $CLASSIC_TEST'
        let env = vec![("CLASSIC_TEST".to_string(), "from-spawn".to_string())];
        let h = exec_command(
            1,
            &argv(&["/bin/sh", "-c", "echo $CLASSIC_TEST"]),
            &env,
            None,
        )
        .await
        .unwrap();
        let mut out = Vec::new();
        let mut stdout_rx = h.stdout;
        while let Some(chunk) = stdout_rx.recv().await {
            if chunk.is_empty() {
                break;
            }
            out.extend(chunk);
        }
        h.wait.await.unwrap().unwrap();
        assert_eq!(&out, b"from-spawn\n");
    }

    #[tokio::test]
    async fn caller_env_not_inherited() {
        // PATH is virtually always set in the parent shell. Without env
        // forwarding it should be unset in the child.
        std::env::set_var("CLASSIC_LEAK_PROBE", "should-not-appear");
        let h = exec_command(
            1,
            &argv(&["/bin/sh", "-c", "echo \"${CLASSIC_LEAK_PROBE:-unset}\""]),
            &[],
            None,
        )
        .await
        .unwrap();
        let mut out = Vec::new();
        let mut stdout_rx = h.stdout;
        while let Some(chunk) = stdout_rx.recv().await {
            if chunk.is_empty() {
                break;
            }
            out.extend(chunk);
        }
        h.wait.await.unwrap().unwrap();
        assert_eq!(&out, b"unset\n");
    }

    #[tokio::test]
    async fn signal_kill_reports_signal() {
        // Spawn a sleeper, send SIGKILL via /bin/kill targeting our own
        // child. Easier: have sh -c 'kill -9 $$'.
        let h = exec_command(
            1,
            &argv(&["/bin/sh", "-c", "kill -9 $$"]),
            &[],
            None,
        )
        .await
        .unwrap();
        let info = h.wait().await.unwrap();
        assert_eq!(info.code, None);
        assert_eq!(info.signal, Some(9));
    }
}
