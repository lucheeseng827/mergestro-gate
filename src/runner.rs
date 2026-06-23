// SPDX-License-Identifier: Apache-2.0
//! Thin abstraction over external process execution.
//!
//! Every subprocess the gate spawns (`git`, `cargo`, `cargo-mutants`) goes
//! through [`CommandRunner`]. The real implementation just wraps
//! [`std::process::Command`]; tests substitute a scripted runner so the
//! orchestration logic can be exercised without a toolchain.

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::Command;

/// Captured result of a single process invocation.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Process exit code, or `None` if the process was killed by a signal.
    pub code: Option<i32>,
    /// Whether the process exited successfully (status `0`).
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    /// Combined stdout + stderr, handy for surfacing failures to the user.
    pub fn combined(&self) -> String {
        let mut s = self.stdout.clone();
        if !self.stderr.is_empty() {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&self.stderr);
        }
        s
    }
}

/// Abstracts spawning a child process so the pipeline can be unit-tested.
pub trait CommandRunner {
    /// Run `program` with `args` in `cwd`, capturing stdout/stderr.
    fn run(&self, program: &str, args: &[&str], cwd: &Path) -> io::Result<CommandOutput>;

    /// Whether `program` is resolvable on this machine (e.g. `cargo-mutants`).
    fn is_available(&self, program: &str) -> bool;
}

/// Production runner backed by the OS.
pub struct RealRunner;

impl CommandRunner for RealRunner {
    fn run(&self, program: &str, args: &[&str], cwd: &Path) -> io::Result<CommandOutput> {
        let output = Command::new(program)
            .args(args.iter().map(OsStr::new))
            .current_dir(cwd)
            .output()?;
        Ok(CommandOutput {
            code: output.status.code(),
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn is_available(&self, program: &str) -> bool {
        // `--version` is cheap and supported by git/cargo/cargo-mutants.
        Command::new(program)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A scripted runner for unit tests: maps `(program, args-substring)` to a
    //! canned [`CommandOutput`].
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct ScriptedRunner {
        /// FIFO of responses, returned in call order.
        responses: Mutex<VecDeque<io::Result<CommandOutput>>>,
        available: Mutex<Vec<String>>,
    }

    impl ScriptedRunner {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn push_ok(&self, stdout: &str) -> &Self {
            self.responses.lock().unwrap().push_back(Ok(CommandOutput {
                code: Some(0),
                success: true,
                stdout: stdout.to_string(),
                stderr: String::new(),
            }));
            self
        }

        pub fn push_fail(&self, code: i32, stderr: &str) -> &Self {
            self.responses.lock().unwrap().push_back(Ok(CommandOutput {
                code: Some(code),
                success: false,
                stdout: String::new(),
                stderr: stderr.to_string(),
            }));
            self
        }

        #[allow(dead_code)]
        pub fn mark_available(&self, program: &str) -> &Self {
            self.available.lock().unwrap().push(program.to_string());
            self
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run(&self, _program: &str, _args: &[&str], _cwd: &Path) -> io::Result<CommandOutput> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    Err(io::Error::other(
                        "ScriptedRunner: no more scripted responses",
                    ))
                })
        }

        fn is_available(&self, program: &str) -> bool {
            self.available.lock().unwrap().iter().any(|p| p == program)
        }
    }
}
