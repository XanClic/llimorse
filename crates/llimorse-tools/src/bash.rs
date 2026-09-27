//! Process execution tools.

use super::ToolGate;
use anyhow::{Result, anyhow};
use helpers::TruncatedDisplay;
use llimorse::{Agent, CallableTool};
use std::fmt;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use tokio::process::Command;

llimorse::tool! {
    'name: "bash";

    /// Execute the given command line through a shell.
    #[derive(Debug)]
    'params: pub struct BashParams {
        /// Command line to execute
        command_line: String,
    }

    /// Result of the command
    #[derive(Debug)]
    'result: pub struct BashResult {
        /// The integer exit code returned by the command
        exit_code: isize,

        /// Stdout output
        stdout: String,

        /// Stderr output
        #[serde(default, skip_serializing_if = "str::is_empty")]
        stderr: String,
    }

    /// Execute commands through a shell. The shell is non-interactive: there is no terminal and no
    /// stdin, so anything that would prompt (passwords, pagers, ...) fails immediately instead of
    /// waiting.
    ///
    /// The command runs asynchronously: awaiting `execute` does not block the runtime, and
    /// dropping the future (e.g. when the tool call is interrupted) kills the child.
    #[derive(Debug)]
    'state: pub struct Bash<G: ToolGate> {
        /// Gate for receiving permissions to execute bash commands
        gate: G,
    }
}

impl<G: ToolGate> Bash<G> {
    /// Create a new bash tool, gated by `gate`
    pub fn new(gate: G) -> Self {
        Bash { gate }
    }
}

impl fmt::Display for BashParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.command_line)
    }
}

impl fmt::Display for BashResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "exit_code={} stdout={:?}",
            self.exit_code,
            self.stdout.truncated_display(100)
        )?;
        if !self.stderr.is_empty() {
            write!(f, " stderr={:?}", self.stderr.truncated_display(100))?;
        }
        Ok(())
    }
}

/// Build the command for executing `command_line` through a shell.
///
/// The shell is decidedly non-interactive. The child is placed in a new session (`setsid`), so it
/// has no controlling terminal: programs that prompt on a tty (git, ssh, ...) open `/dev/tty`
/// directly, bypassing stdin, and a blocked read on it would stop the whole command with
/// `SIGTTIN`. With no terminal to prompt on, they fail fast instead.  The remaining knobs cover
/// prompt paths that don't use a tty at all.
///
/// `kill_on_drop` is enabled so that dropping the future awaiting the command's output kills the
/// child, letting an interrupted tool call abort a still-running command.
fn shell_command(command_line: &str) -> Command {
    let mut cmd = Command::new("bash");
    cmd.args(["-c", command_line]);
    #[cfg(unix)]
    {
        // `pre_exec` runs in the child between fork and exec, where only async-signal-safe calls
        // are allowed; `setsid` is one of them.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    // git: never prompt for credentials (also gives a clearer error)
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    // ssh: never invoke an askpass program
    cmd.env("SSH_ASKPASS_REQUIRE", "never");
    // No terminal and no stdin: anything that would prompt fails immediately instead of waiting.
    // (`tokio::process::Command::output`, unlike its std counterpart, does not null stdin.)
    cmd.stdin(std::process::Stdio::null());
    // Dropping the future awaiting the output kills the child
    cmd.kill_on_drop(true);
    cmd
}

impl<G: ToolGate> CallableTool for Bash<G> {
    async fn execute(&self, _agent: &Agent, params: BashParams) -> Result<BashResult> {
        self.gate
            .permitted(&params)
            .await
            .map_err(|e| anyhow!("Bash tool call rejected: {e}"))?;

        let output = shell_command(&params.command_line)
            .output()
            .await
            .map_err(|err| anyhow!("Failed to execute bash: {err}"))?;

        #[cfg(unix)]
        let exit_code = output
            .status
            .code()
            .or_else(|| output.status.signal().map(|s| -s));
        #[cfg(not(unix))]
        let exit_code = output.status.code();

        Ok(BashResult {
            exit_code: exit_code.unwrap_or(-1) as isize,
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}
