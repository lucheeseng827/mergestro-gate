// SPDX-License-Identifier: Apache-2.0
//! Determinism pre-flight.
//!
//! Mutation results are only meaningful if the baseline suite is *green* and
//! *stable*. A flaky suite produces phantom survivors (the test that would
//! have caught the mutant was itself failing) and phantom catches. So before
//! mutating anything we run the suite N times and require every run to pass.
//! Any failure, or any flip between runs, suppresses the mutation phase.
//!
//! N defaults to 1: that proves the suite green, which is what the mutation
//! needs, and it is the only suite run before the first mutant — a green
//! pre-flight is also why cargo-mutants' own baseline is always skipped
//! (`mutants::mutation_args`). A flip needs two runs to show, so `N >= 2` is
//! how a team with a flaky suite keeps that protection, at one suite run each.

use anyhow::{Context, Result};

use crate::config::Config;
use crate::report::PreflightOutcome;
use crate::runner::CommandRunner;

/// Run the (Rust) suite `cfg.preflight_runs` times and judge stability.
pub fn run(runner: &dyn CommandRunner, cfg: &Config) -> Result<PreflightOutcome> {
    run_command(
        runner,
        &cfg.test_command,
        &cfg.repo,
        cfg.preflight_runs,
        cfg.skip_preflight,
    )
}

/// Run an arbitrary `command` (program first) `runs` times in `repo`, judging
/// determinism. Shared by the Rust and Python paths.
pub fn run_command(
    runner: &dyn CommandRunner,
    command: &[String],
    repo: &std::path::Path,
    runs: u32,
    skip: bool,
) -> Result<PreflightOutcome> {
    if skip {
        return Ok(PreflightOutcome::Skipped);
    }

    let program = &command[0];
    let args: Vec<&str> = command[1..].iter().map(String::as_str).collect();

    let mut prior_success: Option<bool> = None;
    for run_idx in 1..=runs {
        let out = runner
            .run(program, &args, repo)
            .with_context(|| format!("running pre-flight suite (run {run_idx})"))?;

        // A flip relative to a previous run means flakiness — untrustworthy.
        if let Some(prev) = prior_success {
            if prev != out.success {
                return Ok(PreflightOutcome::Unstable {
                    detail: format!(
                        "suite result changed between runs (run {} success={})",
                        run_idx, out.success
                    ),
                });
            }
        }
        prior_success = Some(out.success);

        if !out.success {
            return Ok(PreflightOutcome::Failed {
                run: run_idx,
                detail: first_meaningful_line(&out.combined()),
            });
        }
    }

    Ok(PreflightOutcome::Passed { runs })
}

/// Pull the first non-empty line of output for a compact failure detail.
fn first_meaningful_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("suite exited non-zero")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::test_support::ScriptedRunner;

    fn cfg(runs: u32) -> Config {
        Config {
            preflight_runs: runs,
            ..Config::default()
        }
    }

    #[test]
    fn skipped_when_configured() {
        let runner = ScriptedRunner::new();
        let mut c = cfg(2);
        c.skip_preflight = true;
        matches!(run(&runner, &c).unwrap(), PreflightOutcome::Skipped)
            .then_some(())
            .unwrap();
    }

    #[test]
    fn green_across_runs_passes() {
        let runner = ScriptedRunner::new();
        runner.push_ok("ok").push_ok("ok");
        let outcome = run(&runner, &cfg(2)).unwrap();
        assert!(matches!(outcome, PreflightOutcome::Passed { runs: 2 }));
    }

    #[test]
    fn red_suite_fails_fast() {
        let runner = ScriptedRunner::new();
        runner.push_fail(101, "test foo ... FAILED");
        let outcome = run(&runner, &cfg(3)).unwrap();
        match outcome {
            PreflightOutcome::Failed { run, detail } => {
                assert_eq!(run, 1);
                assert!(detail.contains("FAILED"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn flip_between_runs_is_unstable() {
        let runner = ScriptedRunner::new();
        runner.push_ok("ok").push_fail(101, "flaky");
        let outcome = run(&runner, &cfg(2)).unwrap();
        assert!(matches!(outcome, PreflightOutcome::Unstable { .. }));
    }
}
