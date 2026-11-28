//! Shell child process management with WAYLAND_DISPLAY propagation and clean reaping.

use std::process::{Child, Command, ExitStatus};

use anyhow::Context;
use tracing::{debug, info, warn};

/// Manages the lifecycle of the launched shell client process.
#[derive(Debug)]
pub struct ShellProcess {
    program: String,
    child: Option<Child>,
}

impl ShellProcess {
    /// Spawn `program` with `WAYLAND_DISPLAY` set to `socket_name` (inheriting `XDG_RUNTIME_DIR`).
    pub fn spawn(program: &str, socket_name: &str) -> anyhow::Result<Self> {
        info!(program, socket_name, "Spawning shell process");
        let child = Command::new(program)
            .env("WAYLAND_DISPLAY", socket_name)
            .spawn()
            .with_context(|| format!("failed to spawn shell process `{program}`"))?;

        Ok(Self {
            program: program.to_string(),
            child: Some(child),
        })
    }

    /// Poll whether the child process has exited without blocking.
    ///
    /// Returns `Ok(Some(status))` once when the child exits, and `Ok(None)` if it is still
    /// running or has already been reaped.
    pub fn try_wait(&mut self) -> anyhow::Result<Option<ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };

        match child.try_wait() {
            Ok(Some(status)) => {
                info!(program = %self.program, %status, "Shell process exited");
                self.child = None;
                Ok(Some(status))
            }
            Ok(None) => Ok(None),
            Err(err) => Err(err).context("failed to poll shell process status"),
        }
    }

    /// Terminate and reap the shell process if it is still running.
    pub fn terminate(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };

        match child.try_wait() {
            Ok(Some(status)) => {
                debug!(program = %self.program, %status, "Shell process already exited");
            }
            Ok(None) => {
                info!(program = %self.program, pid = child.id(), "Terminating shell process");
                if let Err(err) = child.kill() {
                    warn!(program = %self.program, error = %err, "Failed to kill shell process");
                }
                if let Err(err) = child.wait() {
                    warn!(program = %self.program, error = %err, "Failed to wait on shell process");
                }
            }
            Err(err) => {
                warn!(program = %self.program, error = %err, "Failed to check shell process before termination");
            }
        }
    }
}

impl Drop for ShellProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}
