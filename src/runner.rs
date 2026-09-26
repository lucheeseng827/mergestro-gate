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
use std::time::{Duration, Instant};

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

    /// Run like [`run`](Self::run), but stop the process once `limit` has
    /// elapsed. Returns the captured output and whether the limit stopped it.
    ///
    /// Used for the mutation budget: `cargo-mutants` writes each mutant's
    /// outcome as it finishes, so a run stopped at the deadline still leaves
    /// every completed result readable. The default never stops anything, which
    /// is what a scripted runner wants unless a test says otherwise.
    fn run_with_limit(
        &self,
        program: &str,
        args: &[&str],
        cwd: &Path,
        limit: Duration,
    ) -> io::Result<(CommandOutput, bool)> {
        let _ = limit;
        self.run(program, args, cwd).map(|out| (out, false))
    }

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

    fn run_with_limit(
        &self,
        program: &str,
        args: &[&str],
        cwd: &Path,
        limit: Duration,
    ) -> io::Result<(CommandOutput, bool)> {
        use std::io::Read;
        use std::process::Stdio;

        // A budget too large to fall on a representable instant is an input
        // error, refused before anything starts. It is not "no limit": that is
        // `None` upstream, and never reaches here.
        let deadline = Instant::now()
            .checked_add(limit)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "budget is too large"))?;
        let mut child = Command::new(program)
            .args(args.iter().map(OsStr::new))
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        // Drain both pipes on their own threads: a child that fills a pipe
        // nobody reads blocks forever, and the deadline would then measure that.
        let drain = |mut pipe: Box<dyn Read + Send>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = pipe.read_to_end(&mut buf);
                String::from_utf8_lossy(&buf).into_owned()
            })
        };
        let stdout = drain(Box::new(child.stdout.take().expect("piped stdout")));
        let stderr = drain(Box::new(child.stderr.take().expect("piped stderr")));

        let mut stopped = false;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if !stopped && Instant::now() >= deadline {
                stopped = true;
                // Ask first: cargo-mutants handles SIGTERM by stopping its test
                // processes and keeping what it has written. Force only if it
                // has not gone within the grace period.
                terminate(&child);
                let grace = Instant::now()
                    + if cfg!(unix) {
                        Duration::from_secs(15)
                    } else {
                        Duration::ZERO // nothing was asked, so nothing to wait for
                    };
                while child.try_wait()?.is_none() && Instant::now() < grace {
                    std::thread::sleep(Duration::from_millis(100));
                }
                if child.try_wait()?.is_none() {
                    force_kill(&mut child);
                }
                continue;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let out = CommandOutput {
            code: status.code(),
            success: status.success(),
            stdout: stdout.join().unwrap_or_default(),
            stderr: stderr.join().unwrap_or_default(),
        };
        Ok((out, stopped))
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

/// Ask `child` to stop. SIGTERM on Unix, where the mutation run happens;
/// elsewhere there is only the hard kill.
fn terminate(child: &std::process::Child) {
    // Through `sh`, not a `kill` program: slim images (`rust:*-slim`, a common
    // CI base) ship no /bin/kill, only the shell builtin. Spawning a missing
    // `kill` fails silently, SIGTERM is never sent, and the stop falls through
    // to the hard kill after the full grace period. The pid is ours and numeric.
    #[cfg(unix)]
    {
        let _ = Command::new("sh")
            .args(["-c", &format!("kill -TERM {}", child.id())])
            .status();
    }
    #[cfg(not(unix))]
    {
        let _ = child;
    }
}

/// Kill `child` and what it started, when asking did not stop it in time.
///
/// cargo-mutants starts each build and test in a process group of its own and,
/// asked with SIGTERM, stops those groups itself. Killed outright it cannot, so
/// they would outlive it: tests still running, build directories still in use.
/// Signalling `child`'s own group would not reach them either, being other
/// groups. So on Unix: note its children, kill it, then kill each child's group
/// (and the child, should one not lead a group).
fn force_kill(child: &mut std::process::Child) {
    #[cfg(unix)]
    let children = child_pids(child.id());
    let _ = child.kill();
    #[cfg(unix)]
    for pid in children {
        // Through `sh`, as in `terminate`: slim images have no `kill` program.
        // And no `--`: dash's builtin rejects it, then signals nothing at all.
        let _ = Command::new("sh")
            .args(["-c", &format!("kill -KILL -{pid} {pid}")])
            .status();
    }
}

/// The processes whose parent is `pid`: read from /proc where there is one
/// (Linux, slim images included, which ship no procps), else `pgrep -P`.
#[cfg(unix)]
fn child_pids(pid: u32) -> Vec<u32> {
    if let Ok(entries) = std::fs::read_dir("/proc") {
        return entries
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            .filter(|&candidate| parent_of(candidate) == Some(pid))
            .collect();
    }
    Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// `pid`'s parent, from `/proc/<pid>/stat`. The command name there sits in
/// parentheses and may hold spaces or parentheses of its own, so the fields are
/// read from after the last `)`: the state, then the parent's pid.
#[cfg(unix)]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = stat[stat.rfind(')')? + 1..].split_whitespace();
    fields.next()?;
    fields.next()?.parse().ok()
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
        /// For `run_with_limit`: whether each call in turn was stopped by its
        /// limit. Unscripted calls ran to completion.
        stopped: Mutex<VecDeque<bool>>,
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

        /// Make the next `run_with_limit` call report that its limit stopped it
        /// (its output still comes from the response queue).
        #[allow(dead_code)]
        pub fn stop_next_at_limit(&self) -> &Self {
            self.stopped.lock().unwrap().push_back(true);
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

        fn run_with_limit(
            &self,
            program: &str,
            args: &[&str],
            cwd: &Path,
            _limit: Duration,
        ) -> io::Result<(CommandOutput, bool)> {
            let out = self.run(program, args, cwd)?;
            let stopped = self.stopped.lock().unwrap().pop_front().unwrap_or(false);
            Ok((out, stopped))
        }

        fn is_available(&self, program: &str) -> bool {
            self.available.lock().unwrap().iter().any(|p| p == program)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    /// Whether `pid` is still running (a zombie awaiting its reaper is not).
    /// From /proc where there is one, so these tests need no procps either.
    fn running(pid: &str) -> bool {
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat
                .rsplit_once(')')
                .map(|(_, rest)| rest.trim_start().chars().take(1).collect::<String>())
                .unwrap_or_default(),
            Err(_) if std::path::Path::new("/proc/self").exists() => String::new(),
            Err(_) => {
                let out = Command::new("ps")
                    .args(["-o", "stat=", "-p", pid])
                    .output()
                    .expect("ps");
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
        };
        !stat.is_empty() && !stat.starts_with('Z')
    }

    #[test]
    fn run_with_limit_refuses_a_budget_it_cannot_represent() {
        let err = RealRunner
            .run_with_limit("true", &[], Path::new("."), Duration::from_secs(u64::MAX))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn run_with_limit_stops_a_run_at_its_limit_and_leaves_a_shorter_one_alone() {
        // Past the limit it is asked to stop, and does: well inside the 15 s
        // before a hard kill.
        let start = Instant::now();
        let (_, stopped) = RealRunner
            .run_with_limit("sleep", &["30"], Path::new("."), Duration::from_millis(200))
            .unwrap();
        assert!(stopped);
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "took {:?}",
            start.elapsed()
        );

        // Within the limit it runs to the end, and is not reported as stopped.
        let (out, stopped) = RealRunner
            .run_with_limit("sleep", &["0.3"], Path::new("."), Duration::from_secs(30))
            .unwrap();
        assert!(!stopped);
        assert!(out.success);
    }

    #[test]
    fn force_kill_takes_the_process_groups_its_children_lead() {
        // Shaped like cargo-mutants: the child starts a process group of its
        // own (setsid), holding a process of its own. Killing the parent alone
        // would leave that `sleep` running.
        let mut parent = Command::new("sh")
            .args(["-c", "setsid sh -c 'sleep 60 & echo $!; wait' & wait"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("sh");
        let mut line = String::new();
        BufReader::new(parent.stdout.take().expect("piped stdout"))
            .read_line(&mut line)
            .expect("the grandchild's pid");
        let sleeper = line.trim().to_string();
        assert!(running(&sleeper), "the process tree did not start");

        force_kill(&mut parent);
        let _ = parent.wait();
        let deadline = Instant::now() + Duration::from_secs(5);
        while running(&sleeper) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!running(&sleeper), "the child's process group outlived it");
    }
}
