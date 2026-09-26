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
use crate::turnover_lane;
use crate::{
    convention, debt, diff, docs_gate, engine, mcp_gate, mutants, security, slop, verdict,
    weakened_tests, zero_assertion,
};

/// Run the full gate, returning the report (verdict included).
pub fn run(runner: &dyn CommandRunner, cfg: &Config, work_dir: &Path) -> Result<GateReport> {
    let started = Instant::now();
    cfg.validate()?;

    // Stage 0: diff scope (pure-Rust, via gix).
    let scope = diff::compute_scope(&cfg.repo, &cfg.base_ref, &cfg.head_ref, work_dir)?;

    let mut report = GateReport::new(&cfg.base_ref, &cfg.head_ref);
    report.shard = cfg.shard.clone();
    report.base_commit = scope.base_commit.clone();
    report.head_commit = scope.head_commit.clone();
    report.test_settings = format!("tool={};timeout={}s", cfg.test_tool, cfg.timeout_secs);
    report.changed_rust_files = scope.changed_rust_files.clone();
    // Narrowing only happens when the changed files resolve to a package: with
    // none, cargo-mutants mutates and tests the whole workspace anyway.
    report.rust_tests_changed_crate_only = cfg.test_changed_package_only
        && !mutants::changed_packages(&cfg.repo, &scope.changed_rust_files).is_empty();
    report.changed_python_files = scope
        .changed_python_files
        .iter()
        .map(|c| c.path.clone())
        .collect();
    report.changed_js_files = scope
        .changed_js_files
        .iter()
        .map(|c| c.path.clone())
        .collect();
    report.changed_go_files = scope
        .changed_go_files
        .iter()
        .map(|c| c.path.clone())
        .collect();
    report.changed_jvm_files = scope
        .changed_jvm_files
        .iter()
        .map(|c| c.path.clone())
        .collect();

    let diff_text = std::fs::read_to_string(&scope.diff_path).unwrap_or_default();

    // A sharded run's lanes run on shard 1 alone, like the non-Rust engines:
    // `merge-reports` keeps shard 1's lane results, so a lane that ran on a
    // later shard too — an MCP probe, say — could fail there and be dropped.
    let lanes_here = owns_lanes(cfg);

    // MCP lane: the only lane that runs the artifact rather than reading the
    // diff, and the only one that scopes itself by declared path rather than by
    // language. It therefore runs *before* the language short-circuit below — a
    // server whose surface this gate does not otherwise recognise (a config
    // file, a schema, a language with no mutation engine) is exactly the change
    // most likely to break it silently.
    if lanes_here {
        report.mcp = mcp_gate::run(runner, cfg, &scope.all_changed_files);
    }

    // Turnover lane: reads history, not the diff, and has its own language set,
    // so it also runs before the language short-circuit. A missing baseline is
    // a skipped lane with a hint, never a block (see `turnover_lane`).
    if lanes_here && cfg.turnover.enabled {
        report.turnover = Some(turnover_lane::run(cfg));
    }

    // Weakened-test lane: also before the short-circuit. Deleting a test file
    // leaves no changed Rust file on the head side, so the language check below
    // would end the run before any later lane saw the deletion.
    if lanes_here {
        let weakened = weakened_tests::scan(&scope.rust_versions);
        if !weakened.findings.is_empty() {
            report.weakened_tests = Some(weakened);
        }
    }

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
    if lanes_here && !diff_text.is_empty() {
        // Phase 4: debt-delta (net complexity/duplication/coupling).
        report.debt = Some(debt::from_unified_diff(&diff_text));

        // Docs lane: the documentation flow — code changes must keep the
        // touched module inside the documentation standard, and reference
        // docs must move with new config/API surface. Whole-diff scoped
        // (any language), like debt.
        let report_docs = docs_gate::scan(&cfg.repo, &diff_text);
        if !report_docs.findings.is_empty() {
            report.docs = Some(report_docs);
        }

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

    // Zero-assertion static pre-check (Rust): independent of suite health, so it
    // runs even when mutation is later suppressed.
    if lanes_here && cfg.check_zero_assertion_tests && !scope.changed_rust_files.is_empty() {
        report.zero_assertion_tests =
            zero_assertion::scan_files(&cfg.repo, &scope.changed_rust_files);
    }

    // Each registered engine mutates the surface it claims. Engines run in
    // order; the first to run owns the shared pre-flight slot. Per-engine
    // pre-flights are independent — a red suite for one language suppresses only
    // that language's mutation.
    for engine in engine::default_engines() {
        if !engine.applies(&scope) || !runs_on_this_shard(cfg, engine.language()) {
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
        report.not_tested_budget += run.results.not_tested;
        if let Some(fingerprint) = run.selection {
            report.selection = fingerprint;
        }
        report.survivor_occurrence.extend(run.occurrences);
        report.tested += run.results.tested();
        report.survivors.extend(run.results.survivors);
    }

    Ok(finalize(report, cfg, started))
}

/// Only the Rust engine shards; the others run whole on shard 1 alone, so
/// `merge-reports` does not count their results once per shard.
fn runs_on_this_shard(cfg: &Config, language: &str) -> bool {
    language == "Rust" || owns_lanes(cfg)
}

/// Unsharded, or shard 1: the run that owns everything but the Rust mutants.
fn owns_lanes(cfg: &Config) -> bool {
    cfg.shard_index().is_none_or(|(k, _)| k == 1)
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
    fn only_rust_shards_and_every_other_engine_runs_on_shard_one_alone() {
        let with = |shard: Option<&str>| Config {
            shard: shard.map(Into::into),
            ..Config::default()
        };
        // Unsharded: everything runs.
        assert!(runs_on_this_shard(&with(None), "Python"));
        // Shard 1 runs the other engines too; later shards only Rust.
        assert!(runs_on_this_shard(&with(Some("1/3")), "Python"));
        assert!(!runs_on_this_shard(&with(Some("2/3")), "Python"));
        assert!(!runs_on_this_shard(&with(Some("3/3")), "JS/TS"));
        assert!(runs_on_this_shard(&with(Some("3/3")), "Rust"));
    }

    #[test]
    fn a_pr_that_only_deletes_a_test_file_is_still_reported() {
        // No changed Rust file survives on the head side, so the language
        // short-circuit ends the run early — the weakened-test lane must have
        // run before it, or this PR passes with nothing said.
        let work = tempdir().unwrap();
        let dir = work.path();
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.email", "t@t.io"]);
        git(dir, &["config", "user.name", "t"]);
        write_files(
            dir,
            &[
                (
                    "src/lib.rs",
                    "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
                ),
                (
                    "tests/math.rs",
                    "#[test]\nfn adds() {\n    assert_eq!(demo::add(2, 3), 5);\n}\n",
                ),
            ],
        );
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "--no-gpg-sign", "-m", "base"]);
        git(dir, &["rm", "-q", "tests/math.rs"]);
        git(
            dir,
            &["commit", "-q", "--no-gpg-sign", "-m", "drop the red test"],
        );

        let runner = ScriptedRunner::new(); // nothing to mutate, nothing runs
        let mut cfg = cfg_for(dir);
        cfg.turnover.enabled = false;
        let report = run(&runner, &cfg, dir).unwrap();
        assert!(report.changed_rust_files.is_empty());
        let w = report
            .weakened_tests
            .expect("the deleted test must be reported");
        assert_eq!(w.findings.len(), 1);
        assert_eq!(w.findings[0].rule, "test-removed");
        assert_eq!(w.findings[0].file, "tests/math.rs");
        // Advisory by default: reported, not blocking.
        assert_eq!(report.verdict, crate::report::Verdict::Pass);
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
            &[(
                "src/lib.rs",
                "use serde::Serialize;\nuse ghost_crate::Thing;\n",
            )],
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

    /// An evidence run as `specprobe` prints it in gate mode.
    fn specprobe_stdout(failed: &[&str]) -> String {
        let blocking: Vec<serde_json::Value> = failed
            .iter()
            .map(|id| {
                serde_json::json!({
                    "check": id,
                    "severity": "critical",
                    "outcome": "fail",
                    "reason": "failed a Critical check",
                    "detail": "stopped answering after a truncated frame",
                })
            })
            .collect();
        serde_json::json!({
            "source": "specprobe_negative",
            "spec_version": "2025-11-25",
            "suite_version": "specprobe/0.1.0",
            "results": [],
            "gate": {
                "fail_on": "critical",
                "blocking": blocking,
                "tally": {
                    "scored": 17, "passed": 17 - failed.len(), "failed": failed.len(),
                    "errored": 0, "skipped": 17, "skipped_checks": [],
                },
            },
        })
        .to_string()
    }

    fn cfg_with_mcp_server(dir: &Path) -> Config {
        Config {
            mcp_servers: vec![crate::config::McpServerTarget {
                name: "acme".into(),
                paths: vec!["servers/acme".into()],
                dir: None,
                build: Vec::new(),
                command: vec!["node".into(), "servers/acme/server.js".into()],
                spec_version: "2025-11-25".into(),
                timeout_secs: 20,
                elicit_tool: None,
            }],
            ..cfg_for(dir)
        }
    }

    #[test]
    fn a_wedge_regression_in_the_repos_own_mcp_server_blocks_the_merge() {
        // The thesis, end to end: a PR that makes the repo's own MCP server stop
        // answering after a malformed frame does not merge, and the PR comment
        // names the check that caught it.
        let work = tempdir().unwrap();
        repo_with_change(
            work.path(),
            &[("servers/acme/server.js", "// v1\n")],
            &[(
                "servers/acme/server.js",
                "// v1\n// v2 — drops the reader\n",
            )],
        );
        let runner = ScriptedRunner::new();
        runner.push(1, &specprobe_stdout(&["NP-FRAME-002"]), "gate tripped");

        let report = run(&runner, &cfg_with_mcp_server(work.path()), work.path()).unwrap();

        assert!(report.verdict.is_block(), "{:?}", report.verdict);
        let Verdict::Block { reasons } = &report.verdict else {
            unreachable!()
        };
        assert!(
            reasons.iter().any(|r| r.contains("NP-FRAME-002")),
            "{reasons:?}"
        );
        assert!(report.render_markdown().contains("NP-FRAME-002"));
        // The probe is the first thing spawned: the lane runs before any
        // language engine, so a diff no engine claims still gets probed.
        assert_eq!(runner.calls()[0].program, "specprobe");
    }

    #[test]
    fn the_mcp_lane_runs_even_when_no_language_engine_claims_the_diff() {
        // A change to a server's config or schema touches no file this gate has
        // a mutation engine for, so the language short-circuit would return
        // before the lane ever looked. That is exactly the change most likely to
        // break a server silently.
        let work = tempdir().unwrap();
        repo_with_change(
            work.path(),
            &[("servers/acme/tools.json", "{}\n")],
            &[("servers/acme/tools.json", "{\"a\":1}\n")],
        );
        let runner = ScriptedRunner::new();
        runner.push(1, &specprobe_stdout(&["NP-LIFE-001"]), "");

        let report = run(&runner, &cfg_with_mcp_server(work.path()), work.path()).unwrap();

        assert!(report.changed_rust_files.is_empty());
        assert!(report.mcp.is_some(), "the lane must have run");
        assert!(report.verdict.is_block());
    }

    #[test]
    fn a_clean_probe_leaves_an_otherwise_clean_run_passing() {
        let work = tempdir().unwrap();
        repo_with_change(
            work.path(),
            &[("servers/acme/server.js", "// v1\n")],
            &[("servers/acme/server.js", "// v2\n")],
        );
        let runner = ScriptedRunner::new();
        runner.push(0, &specprobe_stdout(&[]), "");
        let report = run(&runner, &cfg_with_mcp_server(work.path()), work.path()).unwrap();
        assert_eq!(report.verdict, Verdict::Pass);
        // Reported, not silent: "clear" has to be visible as a result.
        assert!(report.render_text().contains("mcp:"));
    }

    /// Renaming a file OUT of a declared server's path must still probe it. The behavioural
    /// claim, end to end: not "the source path is recorded" (`diff.rs` pins that) but "the lane
    /// actually runs". Moving a server's file elsewhere is a change to that server, and if the
    /// scope only saw the destination the probe would be skipped and the gate would go quiet on
    /// exactly the diff most likely to break it.
    #[test]
    fn renaming_a_file_out_of_a_declared_server_still_probes_it() {
        let work = tempdir().unwrap();
        let p = work.path();
        git(p, &["init", "-q"]);
        git(p, &["config", "user.email", "t@t.io"]);
        git(p, &["config", "user.name", "t"]);
        git(p, &["config", "commit.gpgsign", "false"]);
        write_files(
            p,
            &[
                ("servers/acme/tools.json", "{\"a\":1}\n"),
                ("elsewhere/keep.txt", "x\n"),
            ],
        );
        git(p, &["add", "-A"]);
        git(p, &["commit", "-q", "--no-gpg-sign", "-m", "base"]);
        // The destination is outside every declared path — only the SOURCE puts this in scope.
        git(
            p,
            &["mv", "servers/acme/tools.json", "elsewhere/tools.json"],
        );
        git(p, &["commit", "-q", "--no-gpg-sign", "-m", "moved"]);

        let runner = ScriptedRunner::new();
        runner.push(1, &specprobe_stdout(&["NP-FRAME-001"]), "");
        let report = run(&runner, &cfg_with_mcp_server(p), p).unwrap();

        assert!(
            report.mcp.is_some(),
            "the lane must have run on a rename-out"
        );
        assert_eq!(runner.calls()[0].program, "specprobe");
        assert!(report.verdict.is_block(), "{:?}", report.verdict);
    }

    #[test]
    fn a_diff_outside_every_declared_server_never_spawns_the_prober() {
        let work = tempdir().unwrap();
        repo_with_change(
            work.path(),
            &[("docs/guide.md", "a\n")],
            &[("docs/guide.md", "a\nb\n")],
        );
        let runner = ScriptedRunner::new(); // no responses: any spawn would error
        let report = run(&runner, &cfg_with_mcp_server(work.path()), work.path()).unwrap();
        assert!(report.mcp.is_none());
        assert_eq!(report.verdict, Verdict::Pass);
        assert!(runner.calls().is_empty());
    }
}
