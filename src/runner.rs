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
    fn run(&self, program: &str, args: &[&str], cwd: &Path) -> io::Result<CommandOutput> {
        self.run_env(program, args, &[], cwd)
    }

    /// Run `program` with `args` and extra environment variables, in `cwd`.
    ///
    /// This is the required method — [`run`](Self::run) is the no-env case — so a runner cannot
    /// accidentally implement only the env-less path and silently drop the environment. Dropping
    /// it would not fail loudly: the MCP lane configures `specprobe` entirely through env vars,
    /// so a discarded environment would probe *nothing* and report a clean run.
    ///
    /// Entries are layered over the parent environment, last-wins on duplicate keys.
    fn run_env(
        &self,
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
        cwd: &Path,
    ) -> io::Result<CommandOutput>;

    /// Whether `program` is resolvable on this machine (e.g. `cargo-mutants`).
    fn is_available(&self, program: &str) -> bool;
}

/// Production runner backed by the OS.
pub struct RealRunner;

impl CommandRunner for RealRunner {
    fn run_env(
        &self,
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
        cwd: &Path,
    ) -> io::Result<CommandOutput> {
        let mut command = Command::new(program);
        command.args(args.iter().map(OsStr::new)).current_dir(cwd);
        for (k, v) in env {
            command.env(k, v);
        }
        let output = command.output()?;
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

    /// One recorded invocation, so a test can assert *what* was run, not only what came back.
    #[derive(Debug, Clone)]
    pub struct Invocation {
        pub program: String,
        pub args: Vec<String>,
        pub env: Vec<(String, String)>,
        /// Where it ran. The MCP lane's containment guarantee — a PR-editable
        /// config must not steer commands outside the checkout — is only
        /// provable at the spawn site if the spawn site is recorded.
        pub cwd: std::path::PathBuf,
    }

    impl Invocation {
        /// The value of an env var on this invocation, if it was set.
        pub fn env_var(&self, key: &str) -> Option<&str> {
            self.env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        }
    }

    #[derive(Default)]
    pub struct ScriptedRunner {
        /// FIFO of responses, returned in call order.
        responses: Mutex<VecDeque<io::Result<CommandOutput>>>,
        available: Mutex<Vec<String>>,
        /// Every invocation, in call order.
        calls: Mutex<Vec<Invocation>>,
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

        /// A response with an explicit exit code, stdout **and** stderr — the
        /// shape a tool that fails the build while still printing its report
        /// has, which neither `push_ok` nor `push_fail` can express.
        #[allow(dead_code)]
        pub fn push(&self, code: i32, stdout: &str, stderr: &str) -> &Self {
            self.responses.lock().unwrap().push_back(Ok(CommandOutput {
                code: Some(code),
                success: code == 0,
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
            }));
            self
        }

        /// A spawn failure — the program is not on PATH at all, which is a
        /// different thing from a program that ran and exited non-zero.
        #[allow(dead_code)]
        pub fn push_err(&self, message: &str) -> &Self {
            self.responses
                .lock()
                .unwrap()
                .push_back(Err(io::Error::other(message.to_string())));
            self
        }

        #[allow(dead_code)]
        pub fn mark_available(&self, program: &str) -> &Self {
            self.available.lock().unwrap().push(program.to_string());
            self
        }

        /// Every invocation so far, in call order.
        #[allow(dead_code)]
        pub fn calls(&self) -> Vec<Invocation> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run_env(
            &self,
            program: &str,
            args: &[&str],
            env: &[(&str, &str)],
            cwd: &Path,
        ) -> io::Result<CommandOutput> {
            self.calls.lock().unwrap().push(Invocation {
                program: program.to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                env: env
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
                cwd: cwd.to_path_buf(),
            });
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
