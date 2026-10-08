//! The single sanctioned way to run an external Windows tool.
//!
//! **No v0.1 change uses this.** Every change MinWin ships today is implemented
//! against a documented Win32 API, which is the preferred option: structured
//! results, no localised console output to parse, no shell involved. The runner
//! exists because the next validated changes (scheduled-task state, DISM
//! capability queries) have no clean API equivalent, and when that day comes
//! there must be exactly one place in the codebase that spawns a process.
//!
//! Rules this type enforces so they cannot be forgotten at a call site:
//!
//! * The executable is resolved to an absolute path under `%SystemRoot%\
//!   System32` from a fixed allow-list. A bare name such as `schtasks.exe` is
//!   never handed to the OS resolver, so `PATH` cannot be used to substitute a
//!   different binary.
//! * Arguments are passed as a vector, never concatenated into a command line.
//! * No shell. `cmd.exe /C` and PowerShell are not reachable through this API.
//! * The child inherits no stdin and its output is captured, not streamed.
//! * Logging records the executable, the arguments and the exit code. The
//!   environment is never logged.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::core::error::{MinWinError, Result};

/// Windows tools MinWin is allowed to invoke. Anything not listed here cannot
/// be run, which keeps the set auditable at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsTool {
    /// Scheduled task query and state changes.
    SchTasks,
    /// Power configuration, for the few operations `powrprof` does not expose.
    PowerCfg,
    /// Deployment Image Servicing and Management.
    Dism,
}

impl WindowsTool {
    pub fn file_name(self) -> &'static str {
        match self {
            Self::SchTasks => "schtasks.exe",
            Self::PowerCfg => "powercfg.exe",
            Self::Dism => "dism.exe",
        }
    }
}

/// What happened when a tool ran.
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    /// `None` when the process was terminated by a signal rather than exiting.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
}

impl CommandOutcome {
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// A one-line description suitable for a log or an error message.
    pub fn describe(&self) -> String {
        let args: Vec<String> = self
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        format!(
            "{} {} -> {} in {}ms",
            self.executable.display(),
            args.join(" "),
            match self.exit_code {
                Some(code) => code.to_string(),
                None => "terminated".to_string(),
            },
            self.duration.as_millis()
        )
    }

    /// Turns a non-zero exit into a MinWin error that names the tool and keeps
    /// the first line of stderr, which is the part that explains the failure.
    pub fn require_success(&self, operation: &str) -> Result<()> {
        if self.succeeded() {
            return Ok(());
        }
        let detail = first_meaningful_line(&self.stderr)
            .or_else(|| first_meaningful_line(&self.stdout))
            .unwrap_or_else(|| "the tool produced no diagnostic output".to_string());
        Err(MinWinError::windows(
            format!(
                "{operation} using {}",
                self.executable
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| self.executable.display().to_string())
            ),
            detail,
            self.exit_code.unwrap_or(-1) as u32,
        ))
    }
}

fn first_meaningful_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, tool: WindowsTool, arguments: &[&str]) -> Result<CommandOutcome>;
}

#[derive(Debug, Clone)]
pub struct WindowsCommandRunner {
    system32: PathBuf,
}

impl WindowsCommandRunner {
    /// Resolves `%SystemRoot%\System32` once, so individual calls cannot be
    /// redirected by a later environment change.
    pub fn new() -> Result<Self> {
        let system_root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        Ok(Self {
            system32: system_root.join("System32"),
        })
    }

    pub fn with_system32(system32: PathBuf) -> Self {
        Self { system32 }
    }

    pub fn resolve(&self, tool: WindowsTool) -> PathBuf {
        self.system32.join(tool.file_name())
    }
}

impl CommandRunner for WindowsCommandRunner {
    fn run(&self, tool: WindowsTool, arguments: &[&str]) -> Result<CommandOutcome> {
        let executable = self.resolve(tool);
        if !executable.is_absolute() {
            return Err(MinWinError::Unsupported(format!(
                "refusing to run {} because it did not resolve to an absolute path",
                executable.display()
            )));
        }

        let started = Instant::now();
        let output = Command::new(&executable)
            .args(arguments)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| MinWinError::io(format!("run {}", executable.display()), e))?;
        let duration = started.elapsed();

        let outcome = CommandOutcome {
            executable,
            arguments: arguments.iter().map(OsString::from).collect(),
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            duration,
        };
        tracing::debug!(command = %outcome.describe(), "ran a Windows tool");
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(exit_code: Option<i32>, stdout: &str, stderr: &str) -> CommandOutcome {
        CommandOutcome {
            executable: PathBuf::from(r"C:\Windows\System32\schtasks.exe"),
            arguments: vec![OsString::from("/Query"), OsString::from("/TN")],
            exit_code,
            stdout: stdout.into(),
            stderr: stderr.into(),
            duration: Duration::from_millis(12),
        }
    }

    #[test]
    fn tools_resolve_under_system32_not_through_path() {
        let runner = WindowsCommandRunner::with_system32(PathBuf::from(r"C:\Windows\System32"));
        let resolved = runner.resolve(WindowsTool::SchTasks);
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("schtasks.exe"));
        assert!(resolved.starts_with(r"C:\Windows\System32"));
    }

    #[test]
    fn a_zero_exit_is_success() {
        assert!(outcome(Some(0), "", "").succeeded());
        assert!(
            outcome(Some(0), "", "")
                .require_success("query a task")
                .is_ok()
        );
    }

    #[test]
    fn a_failure_keeps_the_first_stderr_line_and_names_the_tool() {
        let error = outcome(Some(1), "", "\n\nERROR: Access is denied.\nmore detail\n")
            .require_success("disable a scheduled task")
            .expect_err("should fail");
        let rendered = error.to_string();
        assert!(rendered.contains("disable a scheduled task"));
        assert!(rendered.contains("schtasks.exe"));
        assert!(rendered.contains("ERROR: Access is denied."));
        assert!(!rendered.contains("more detail"));
    }

    #[test]
    fn a_failure_with_no_stderr_falls_back_to_stdout_then_to_a_plain_statement() {
        let from_stdout = outcome(Some(2), "task not found", "")
            .require_success("query a task")
            .expect_err("should fail");
        assert!(from_stdout.to_string().contains("task not found"));

        let silent = outcome(Some(2), "  \n", "   ")
            .require_success("query a task")
            .expect_err("should fail");
        assert!(silent.to_string().contains("no diagnostic output"));
    }

    #[test]
    fn descriptions_name_the_executable_arguments_and_exit_code() {
        let described = outcome(Some(0), "", "").describe();
        assert!(described.contains("schtasks.exe"));
        assert!(described.contains("/Query /TN"));
        assert!(described.contains("-> 0"));
        assert!(described.contains("12ms"));
    }

    #[test]
    fn a_terminated_process_is_not_treated_as_success() {
        let terminated = outcome(None, "", "");
        assert!(!terminated.succeeded());
        assert!(terminated.describe().contains("terminated"));
    }

    /// Exercises a real process launch. Ignored by default: `cargo test` must
    /// not depend on the host's tools. Run with
    /// `cargo test -- --ignored live_powercfg_query`.
    #[test]
    #[ignore = "launches a real Windows process; read-only but host-dependent"]
    #[cfg(windows)]
    fn live_powercfg_query_is_read_only_and_succeeds() {
        let runner = WindowsCommandRunner::new().expect("runner");
        let outcome = runner
            .run(WindowsTool::PowerCfg, &["/list"])
            .expect("powercfg should run");
        assert!(outcome.succeeded(), "{}", outcome.describe());
        assert!(outcome.stdout.contains("GUID"));
    }
}
