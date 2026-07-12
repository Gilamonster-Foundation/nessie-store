//! The command seam.
//!
//! Every `zfs`/`zpool`/`exportfs`/`chown`/`chmod` invocation goes through a
//! [`CommandRunner`]. Production uses [`SystemRunner`] (real subprocesses); tests
//! use a mock that records argv and returns scripted output, so the exact command
//! lines are asserted without touching a real pool. The backend emits bare argv
//! (no `sudo`) — privilege is the deployment's concern (ZFS delegation or running
//! as root via a narrow sudoers rule).
//!
//! Most commands are short-lived and fully buffered ([`CommandRunner::run`]). The
//! SnapMirror data plane needs **streaming** instead — a binary `zfs send` whose
//! stdout is piped over the network, and a `zfs receive` fed from a network body —
//! so the seam also exposes [`CommandRunner::spawn_stdout`] and
//! [`CommandRunner::run_stdin`]. Both default to "not supported" so only the runners
//! that can stream (the real [`SystemRunner`] and test mocks) implement them.

use std::io::Read;
use std::process::{Child, ChildStdout, Command, Stdio};

use nessie_backend_core::BackendError;

/// The captured result of running a command.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// True if the process exited 0.
    pub success: bool,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
}

/// Runs a command line and captures (or streams) its output.
pub trait CommandRunner: Send + Sync {
    /// Run `argv` (argv[0] is the program) and capture stdout/stderr.
    fn run(&self, argv: &[&str]) -> Result<CommandOutput, BackendError>;

    /// Spawn `argv` and stream its stdout. The returned reader owns the child
    /// process; a non-zero exit surfaces as an I/O error when the stream reaches
    /// EOF, and dropping the reader before EOF kills the child. Defaults to
    /// unsupported.
    fn spawn_stdout(&self, _argv: &[&str]) -> Result<Box<dyn Read + Send>, BackendError> {
        Err(BackendError::FeatureNotSupported {
            capability: "streaming send",
        })
    }

    /// Spawn `argv`, stream `input` into its stdin, wait for it, and return the
    /// number of bytes written. Defaults to unsupported. (The child must not emit a
    /// large volume of stdout while consuming stdin — `zfs receive` does not.)
    fn run_stdin(&self, _argv: &[&str], _input: &mut dyn Read) -> Result<u64, BackendError> {
        Err(BackendError::FeatureNotSupported {
            capability: "streaming receive",
        })
    }
}

impl<T: CommandRunner + ?Sized> CommandRunner for std::sync::Arc<T> {
    fn run(&self, argv: &[&str]) -> Result<CommandOutput, BackendError> {
        (**self).run(argv)
    }
    fn spawn_stdout(&self, argv: &[&str]) -> Result<Box<dyn Read + Send>, BackendError> {
        (**self).spawn_stdout(argv)
    }
    fn run_stdin(&self, argv: &[&str], input: &mut dyn Read) -> Result<u64, BackendError> {
        (**self).run_stdin(argv, input)
    }
}

/// The real runner: `std::process::Command`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, argv: &[&str]) -> Result<CommandOutput, BackendError> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| BackendError::Internal("empty argv".into()))?;
        let out =
            Command::new(program)
                .args(args)
                .output()
                .map_err(|e| BackendError::CommandFailed {
                    command: argv.join(" "),
                    stderr: e.to_string(),
                })?;
        Ok(CommandOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn spawn_stdout(&self, argv: &[&str]) -> Result<Box<dyn Read + Send>, BackendError> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| BackendError::Internal("empty argv".into()))?;
        let mut child = Command::new(program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| BackendError::CommandFailed {
                command: argv.join(" "),
                stderr: e.to_string(),
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BackendError::Internal("child stdout was not captured".into()))?;
        Ok(Box::new(ChildStdoutReader {
            command: argv.join(" "),
            child,
            stdout,
            reaped: false,
        }))
    }

    fn run_stdin(&self, argv: &[&str], input: &mut dyn Read) -> Result<u64, BackendError> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| BackendError::Internal("empty argv".into()))?;
        let command = argv.join(" ");
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| BackendError::CommandFailed {
                command: command.clone(),
                stderr: e.to_string(),
            })?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| BackendError::Internal("child stdin was not captured".into()))?;
        let copied = std::io::copy(input, &mut stdin).map_err(|e| BackendError::CommandFailed {
            command: command.clone(),
            stderr: e.to_string(),
        })?;
        drop(stdin); // close stdin so the child sees EOF
        let out = child
            .wait_with_output()
            .map_err(|e| BackendError::CommandFailed {
                command: command.clone(),
                stderr: e.to_string(),
            })?;
        if !out.status.success() {
            return Err(BackendError::CommandFailed {
                command,
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        Ok(copied)
    }
}

/// A reader over a child's stdout that reaps the child at EOF and reports a
/// non-zero exit as an I/O error. Dropping it before EOF kills the child.
struct ChildStdoutReader {
    command: String,
    child: Child,
    stdout: ChildStdout,
    reaped: bool,
}

impl Read for ChildStdoutReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.reaped {
            return Ok(0);
        }
        let n = self.stdout.read(buf)?;
        if n == 0 {
            self.reaped = true;
            let status = self.child.wait()?;
            if !status.success() {
                let mut stderr = String::new();
                if let Some(mut e) = self.child.stderr.take() {
                    let _ = e.read_to_string(&mut stderr);
                }
                return Err(std::io::Error::other(format!(
                    "`{}` failed ({status}): {}",
                    self.command,
                    stderr.trim()
                )));
            }
        }
        Ok(n)
    }
}

impl Drop for ChildStdoutReader {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn spawn_stdout_streams_child_output() {
        let mut r = SystemRunner
            .spawn_stdout(&["printf", "hello"])
            .expect("spawn printf");
        let mut buf = String::new();
        r.read_to_string(&mut buf).expect("read stdout");
        assert_eq!(buf, "hello");
    }

    #[test]
    fn spawn_stdout_reports_nonzero_exit_at_eof() {
        let mut r = SystemRunner.spawn_stdout(&["false"]).expect("spawn false");
        let mut buf = Vec::new();
        // `false` writes nothing and exits 1, so the read at EOF is an error.
        assert!(r.read_to_end(&mut buf).is_err());
    }

    #[test]
    fn run_stdin_feeds_child_and_counts_bytes() {
        let mut input = std::io::Cursor::new(b"stream-bytes".to_vec());
        let n = SystemRunner
            .run_stdin(&["cat"], &mut input)
            .expect("run cat");
        assert_eq!(n, 12);
    }
}
