//! The command seam.
//!
//! Every `zfs`/`zpool`/`exportfs`/`chown`/`chmod` invocation goes through a
//! [`CommandRunner`]. Production uses [`SystemRunner`] (real subprocesses); tests
//! use a mock that records argv and returns scripted output, so the exact command
//! lines are asserted without touching a real pool. The backend emits bare argv
//! (no `sudo`) — privilege is the deployment's concern (ZFS delegation or running
//! as root via a narrow sudoers rule).

use std::process::Command;

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

/// Runs a command line and captures its output.
pub trait CommandRunner: Send + Sync {
    /// Run `argv` (argv[0] is the program) and capture stdout/stderr.
    fn run(&self, argv: &[&str]) -> Result<CommandOutput, BackendError>;
}

impl<T: CommandRunner + ?Sized> CommandRunner for std::sync::Arc<T> {
    fn run(&self, argv: &[&str]) -> Result<CommandOutput, BackendError> {
        (**self).run(argv)
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
}
