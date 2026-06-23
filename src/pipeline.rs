// SPDX-License-Identifier: Apache-2.0
//! Orchestration: wire diff → pre-flight → mutate → zero-assertion → verdict.
//!
//! Each stage can short-circuit and still produce a coherent [`GateReport`]:
//! nothing relevant changed → nothing to mutate; suite not green → suppress
//! mutation. Language adapters implement [`crate::engine::MutationEngine`] so
//! verdict, severity, debt-delta and telemetry are written once and shared.
//! Every path finalises through a single [`finalize`].

use std::path::Path;
use std::time::Instant;

use anyhow::Result;

use crate::config::Config;
use crate::report::{GateReport, PreflightOutcome};
use crate::runner::CommandRunner;
use crate::{convention, debt, diff, engine, security, slop, verdict, zero_assertion};

/// Run the full gate, returning the report (verdict included).
pub fn run(runner: &dyn CommandRunner, cfg: &Config, work_dir: &Path) -> Result<GateReport> {
    let started = Instant::now();
    cfg.validate()?;

    // Stage 0: diff scope (pure-Rust, via gix).
    let scope = diff::compute_scope(&cfg.repo, &cfg.base_ref, &cfg.head_ref, work_dir)?;

    let mut report = GateReport::new(&cfg.base_ref, &cfg.head_ref);
    report.changed_rust_files = scope.changed_rust_files.clone();
    report.changed_python_files = scope
        .changed_python_files
        .iter()
        .map(|c| c.path.clone())
        .collect();
    report.changed_js_files = scope.changed_js_files.iter().map(|c| c.path.clone()).collect();
    report.changed_go_files = scope.changed_go_files.iter().map(|c| c.path.clone()).collect();
    report.changed_jvm_files = scope.changed_jvm_files.iter().map(|c| c.path.clone()).collect();

    if scope.changed_rust_files.is_empty()
        && scope.changed_python_files.is_empty()
        && scope.changed_js_files.is_empty()
        && scope.changed_go_files.is_empty()
        && scope.changed_jvm_files.is_empty()
    {
        return Ok(finalize(report, cfg, started));
    }

    // Static signals from the diff — independent of the suite's health, so they
    // run even when mutation is later suppressed.
    if let Ok(diff_text) = std::fs::read_to_string(&scope.diff_path) {
        if !diff_text.is_empty() {
            // Phase 4: debt-delta (net complexity/duplication/coupling).
            report.debt = Some(debt::from_unified_diff(&diff_text));

            // Pattern lanes on the added Rust surface (advisory — never change
            // the verdict). Both are static and share the added-line scoping.
            if !scope.changed_rust_files.is_empty() {
                let added = slop::added_lines(&diff_text);
                let report_slop = slop::scan_files(&cfg.repo, &scope.changed_rust_files, &added);
                report.slop = Some(report_slop);
                let report_security =
                    security::scan_files(&cfg.repo, &scope.changed_rust_files, &added);
                if !report_security.findings.is_empty() {
                    report.security = Some(report_security);
                }
                let report_convention =
                    convention::scan_files(&cfg.repo, &scope.changed_rust_files, &added);
                if !report_convention.findings.is_empty() {
                    report.convention = Some(report_convention);
                }
            }
        }
    }

    // Zero-assertion static pre-check (Rust): independent of suite health, so it
    // runs even when mutation is later suppressed.
    if cfg.check_zero_assertion_tests && !scope.changed_rust_files.is_empty() {
        report.zero_assertion_tests =
            zero_assertion::scan_files(&cfg.repo, &scope.changed_rust_files);
    }

    // Each registered engine mutates the surface it claims. Engines run in
    // order; the first to run owns the shared pre-flight slot. Per-engine
    // pre-flights are independent — a red suite for one language suppresses only
    // that language's mutation.
    for engine in engine::default_engines() {
        if !engine.applies(&scope) {
            continue;
        }
        let pf = engine.preflight(runner, cfg)?;
        if matches!(report.preflight, PreflightOutcome::Skipped) {
            report.preflight = pf.clone();
        }
        // A red/flaky suite means survivors can't be trusted — suppress this
        // engine's mutation, but still record the pre-flight above.
        if !pf.is_green() {
            continue;
        }
        if !engine.available(runner, cfg) {
            // Fatal for required engines (Rust), a warning+skip for optional
            // ones (Python PoC) — the engine decides.
            engine.on_unavailable(&scope)?;
            continue;
        }

        let run = engine.analyze(runner, cfg, &scope, work_dir)?;
        report.candidates += run.candidates;
        report.capped_out += run.capped_out;
        report.caught += run.results.caught;
        report.timed_out += run.results.timed_out;
        report.unviable += run.results.unviable;
        report.tested += run.results.tested();
        report.survivors.extend(run.results.survivors);
    }

    Ok(finalize(report, cfg, started))
}

/// Compute the verdict and elapsed time — the single exit point for every path.
fn finalize(mut report: GateReport, cfg: &Config, started: Instant) -> GateReport {
    report.verdict = verdict::decide(&report, cfg);
    report.duration_secs = started.elapsed().as_secs_f64();
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{PreflightOutcome, Verdict};
    use crate::runner::test_support::ScriptedRunner;
    use std::process::Command;
    use tempfile::tempdir;

    /// Run a git command in `dir`, asserting success.
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git available");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Initialise a repo with one base commit, then a second commit that writes
    /// `head_files`. Returns nothing; base is `HEAD~1`, head is `HEAD`.
    fn repo_with_change(dir: &Path, base_files: &[(&str, &str)], head_files: &[(&str, &str)]) {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.email", "t@t.io"]);
        git(dir, &["config", "user.name", "t"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        write_files(dir, base_files);
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "--no-gpg-sign", "-m", "base"]);
        write_files(dir, head_files);
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "--no-gpg-sign", "-m", "head"]);
    }

    fn write_files(dir: &Path, files: &[(&str, &str)]) {
        for (rel, content) in files {
            let p = dir.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, content).unwrap();
        }
    }

    fn cfg_for(dir: &Path) -> Config {
        Config {
            repo: dir.to_path_buf(),
            base_ref: "HEAD~1".into(),
            head_ref: "HEAD".into(),
            preflight_runs: 1,
            ..Config::default()
        }
    }

    #[test]
    fn non_rust_diff_short_circuits_and_passes() {
        let work = tempdir().unwrap();
        repo_with_change(
            work.path(),
            &[("README.md", "a\n")],
            &[("README.md", "a\nb\n")],
        );
        let runner = ScriptedRunner::new(); // no commands should be consumed
        let report = run(&runner, &cfg_for(work.path()), work.path()).unwrap();
        assert!(report.changed_rust_files.is_empty());
        assert_eq!(report.verdict, Verdict::Pass);
        assert!(report.render_text().contains("nothing to mutate"));
    }

    #[test]
    fn red_preflight_suppresses_mutation_and_blocks() {
        let work = tempdir().unwrap();
        repo_with_change(
            work.path(),
            &[("src/lib.rs", "pub fn f(x: i32) -> i32 { x }\n")],
            &[("src/lib.rs", "pub fn f(x: i32) -> i32 { x + 1 }\n")],
        );
        let runner = ScriptedRunner::new();
        runner.push_fail(101, "test failed"); // pre-flight run 1
        let report = run(&runner, &cfg_for(work.path()), work.path()).unwrap();
        assert_eq!(report.changed_rust_files, vec!["src/lib.rs"]);
        assert!(matches!(report.preflight, PreflightOutcome::Failed { .. }));
        assert_eq!(report.candidates, 0); // mutation never ran
                                          // A red suite blocks by default (results can't be trusted).
        assert!(report.verdict.is_block());
    }

    #[test]
    fn zero_assertion_scan_suppressed_when_check_disabled() {
        // When check_zero_assertion_tests is false, the scan must not run even
        // when Rust files changed and the code has zero-assertion patterns.
        // Guards the `cfg.check_zero_assertion_tests && ...` compound condition.
        let work = tempdir().unwrap();
        let theater = "pub fn f() {}\n#[test]\nfn t() { f(); }\n";
        repo_with_change(
            work.path(),
            &[("src/lib.rs", "pub fn f() {}\n")],
            &[("src/lib.rs", theater)],
        );
        let runner = ScriptedRunner::new();
        runner.push_fail(101, "irrelevant"); // stop after pre-flight
        let cfg = Config {
            check_zero_assertion_tests: false,
            ..cfg_for(work.path())
        };
        let report = run(&runner, &cfg, work.path()).unwrap();
        assert!(
            report.zero_assertion_tests.is_empty(),
            "zero-assertion scan must be suppressed when check_zero_assertion_tests is false"
        );
    }

    #[test]
    fn zero_assertion_test_is_surfaced() {
        let work = tempdir().unwrap();
        let theater = "pub fn f() {}\n#[test]\nfn t() { f(); }\n";
        repo_with_change(
            work.path(),
            &[("src/lib.rs", "pub fn f() {}\n")],
            &[("src/lib.rs", theater)],
        );
        let runner = ScriptedRunner::new();
        runner.push_fail(101, "irrelevant"); // stop after pre-flight
        let report = run(&runner, &cfg_for(work.path()), work.path()).unwrap();
        assert_eq!(report.zero_assertion_tests.len(), 1);
        assert_eq!(report.zero_assertion_tests[0].function, "t");
    }

    #[test]
    fn missing_engine_bails_after_green_preflight() {
        // Regression guard: the engine probe runs `cargo mutants --version`
        // (cargo-mutants is a cargo subcommand). When it fails, the gate must
        // bail with the install hint rather than proceeding.
        let work = tempdir().unwrap();
        repo_with_change(
            work.path(),
            &[("src/lib.rs", "pub fn f(x: i32) -> i32 { x }\n")],
            &[("src/lib.rs", "pub fn f(x: i32) -> i32 { x + 1 }\n")],
        );
        let runner = ScriptedRunner::new();
        runner.push_ok("test result: ok"); // pre-flight run 1 → green
        runner.push_fail(101, "error: no such subcommand: `mutants`"); // engine probe
        let err = run(&runner, &cfg_for(work.path()), work.path()).unwrap_err();
        assert!(err.to_string().contains("cargo-mutants not found"));
    }

    #[test]
    fn slop_is_detected_when_rust_files_change() {
        // When the diff adds a redundant wrapper, the slop field must be
        // populated. This guards the `!scope.changed_rust_files.is_empty()` check
        // and the `!report_slop.findings.is_empty()` check together.
        let work = tempdir().unwrap();
        repo_with_change(
            work.path(),
            &[("src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }\n")],
            &[("src/lib.rs",
               "pub fn add(a: i32, b: i32) -> i32 { a + b }\npub fn wrap(a: i32, b: i32) -> i32 { add(a, b) }\n")],
        );
        let runner = ScriptedRunner::new();
        runner.push_fail(101, "test failed"); // stop after pre-flight; slop runs before it
        let report = run(&runner, &cfg_for(work.path()), work.path()).unwrap();
        assert!(!report.changed_rust_files.is_empty());
        assert!(
            report.slop.is_some(),
            "slop must be Some when a wrapper pattern is added in the diff"
        );
        assert!(
            !report.slop.as_ref().unwrap().findings.is_empty(),
            "slop findings must be non-empty for the redundant wrapper"
        );
    }

    #[test]
    fn slop_is_some_with_empty_findings_when_no_patterns() {
        // When the diff contains no slop patterns, report.slop is still Some
        // (always populated when Rust files changed) but with empty findings —
        // ensuring clean-run slop scores (0/100) are included in telemetry.
        let work = tempdir().unwrap();
        repo_with_change(
            work.path(),
            &[("src/lib.rs", "pub fn f(x: i32) -> i32 { x }\n")],
            &[("src/lib.rs", "pub fn f(x: i32) -> i32 { x + 1 }\n")],
        );
        let runner = ScriptedRunner::new();
        runner.push_fail(101, "test failed"); // stop after pre-flight
        let report = run(&runner, &cfg_for(work.path()), work.path()).unwrap();
        assert!(!report.changed_rust_files.is_empty());
        assert!(
            report.slop.is_some(),
            "slop must be Some even when no patterns found (for telemetry completeness)"
        );
        assert!(
            report.slop.as_ref().unwrap().findings.is_empty(),
            "slop findings must be empty when no slop patterns are in the diff"
        );
    }

    #[test]
    fn convention_is_detected_when_hallucinated_import_added() {
        // When the diff adds a `use` of a crate that is not in the manifest,
        // report.convention must be Some with non-empty findings. Guards the
        // `!report_convention.findings.is_empty()` check in pipeline.rs.
        let work = tempdir().unwrap();
        repo_with_change(
            work.path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"demo\"\n[dependencies]\nserde = \"1\"\n",
                ),
                ("src/lib.rs", "use serde::Serialize;\n"),
            ],
            &[("src/lib.rs", "use serde::Serialize;\nuse ghost_crate::Thing;\n")],
        );
        let runner = ScriptedRunner::new();
        runner.push_fail(101, "test failed"); // static signals run before pre-flight
        let report = run(&runner, &cfg_for(work.path()), work.path()).unwrap();
        assert!(!report.changed_rust_files.is_empty());
        assert!(
            report.convention.is_some(),
            "convention must be Some when a hallucinated import is added in the diff"
        );
        assert!(
            !report.convention.as_ref().unwrap().findings.is_empty(),
            "convention findings must be non-empty for the hallucinated import"
        );
    }

    #[test]
    fn security_is_detected_when_hardcoded_secret_added() {
        // When the diff adds a hardcoded-secret line, report.security must be
        // Some with non-empty findings. Guards the `!findings.is_empty()` check
        // that gates setting report.security in pipeline.rs.
        let work = tempdir().unwrap();
        repo_with_change(
            work.path(),
            &[("src/lib.rs", "pub fn f() {}\n")],
            &[(
                "src/lib.rs",
                "pub fn f() {}\nlet token = \"sk-hardcoded-secret-xyz\";\n",
            )],
        );
        let runner = ScriptedRunner::new();
        runner.push_fail(101, "test failed"); // static signals run before pre-flight
        let report = run(&runner, &cfg_for(work.path()), work.path()).unwrap();
        assert!(!report.changed_rust_files.is_empty());
        assert!(
            report.security.is_some(),
            "security must be Some when a hardcoded-secret line is added in the diff"
        );
        assert!(
            !report.security.as_ref().unwrap().findings.is_empty(),
            "security findings must be non-empty for the hardcoded-secret line"
        );
    }
}
