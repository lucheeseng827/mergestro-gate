// SPDX-License-Identifier: Apache-2.0
//! TS/JS mutation via **Stryker** (advisory PoC) — Track A language adapter.
//!
//! Mirrors the Python (cosmic-ray) adapter: drive the engine over the changed
//! files, then feed survivors through the *same* verdict, severity and
//! telemetry as Rust. Stryker has no native line-level `--in-diff`, so we set
//! `mutate` to the changed files and filter the JSON report back to the diff's
//! changed lines — coarser per-file than the Rust `--in-diff` path, the same
//! tradeoff cosmic-ray makes.
//!
//! Stryker is config-driven: we synthesise a minimal config (test runner
//! detected from `package.json`, JSON reporter to a known path) in the work
//! dir and run `stryker run <config>`. The determinism pre-flight is Stryker's
//! own initial test run — it aborts if the baseline suite is red — so the
//! engine adapter reports `Skipped` for the gate's pre-flight.
//!
//! Deferred (vs the Rust path): a per-function cap, real line-level scoping
//! (filtered post-hoc), and JS/TS zero-assertion + AST severity. Requires
//! `stryker` on `PATH` and a detectable `@stryker-mutator/*-runner`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::diff::FileChange;
use crate::mutants::MutationResults;
use crate::report::Mutant;
use crate::runner::CommandRunner;

/// Probe whether Stryker is installed (`stryker --version`).
pub fn is_available(runner: &dyn CommandRunner, repo: &Path) -> bool {
    runner
        .run("stryker", &["--version"], repo)
        .map(|o| o.success)
        .unwrap_or(false)
}

/// Detect the Stryker test-runner plugin from `package.json` (the
/// `@stryker-mutator/<name>-runner` dependency). `None` if none is declared.
pub fn detect_runner(repo: &Path) -> Option<String> {
    let text = std::fs::read_to_string(repo.join("package.json")).ok()?;
    let pkg: serde_json::Value = serde_json::from_str(&text).ok()?;
    for section in ["dependencies", "devDependencies"] {
        if let Some(map) = pkg.get(section).and_then(|v| v.as_object()) {
            for key in map.keys() {
                if let Some(name) = key
                    .strip_prefix("@stryker-mutator/")
                    .and_then(|s| s.strip_suffix("-runner"))
                {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

/// Run Stryker over the changed JS/TS files, returning outcomes scoped to the
/// changed lines.
pub fn run(
    runner: &dyn CommandRunner,
    cfg: &Config,
    changes: &[FileChange],
    work_dir: &Path,
) -> Result<MutationResults> {
    let test_runner = detect_runner(&cfg.repo)
        .context("no @stryker-mutator/*-runner found in package.json")?;

    let report_path = work_dir.join("stryker-report.json");
    let cfg_path = work_dir.join("stryker.conf.json");
    let mutate: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
    std::fs::write(
        &cfg_path,
        stryker_config(
            &test_runner,
            &mutate,
            &report_path.to_string_lossy(),
            cfg.jobs,
            cfg.timeout_secs,
            &work_dir.join(".stryker-tmp").to_string_lossy(),
        ),
    )
    .with_context(|| format!("writing stryker config {}", cfg_path.display()))?;

    // Stryker exits non-zero when survivors push the score below threshold —
    // that's a signal, not an error. The JSON report is the source of truth, so
    // we don't gate on the exit code; we gate on the report existing.
    let cfg_s = cfg_path.to_string_lossy().into_owned();
    let out = runner
        .run("stryker", &["run", &cfg_s], &cfg.repo)
        .context("running `stryker run`")?;

    // Prefer our configured report path; fall back to Stryker's default.
    let json = std::fs::read_to_string(&report_path)
        .or_else(|_| std::fs::read_to_string(cfg.repo.join("reports/mutation/mutation.json")))
        .map_err(|_| {
            anyhow::anyhow!(
                "stryker produced no JSON report (exit {:?}):\n{}",
                out.code,
                out.combined()
            )
        })?;

    let allowed = changes
        .iter()
        .map(|c| (c.path.clone(), c.added_lines.iter().copied().collect()))
        .collect();
    parse_report(&json, &allowed).context("parsing stryker JSON report")
}

/// Build a minimal Stryker config for a diff-scoped run.
fn stryker_config(
    test_runner: &str,
    mutate: &[String],
    report_file: &str,
    jobs: usize,
    timeout_secs: u64,
    temp_dir: &str,
) -> String {
    let mutate_json = serde_json::to_string(mutate).unwrap_or_else(|_| "[]".to_string());
    format!(
        "{{\n  \"testRunner\": {runner},\n  \"mutate\": {mutate},\n  \
         \"reporters\": [\"json\"],\n  \"jsonReporter\": {{ \"fileName\": {report} }},\n  \
         \"coverageAnalysis\": \"perTest\",\n  \"concurrency\": {jobs},\n  \
         \"timeoutMS\": {timeout},\n  \"tempDirName\": {temp}\n}}\n",
        runner = json_str(test_runner),
        mutate = mutate_json,
        report = json_str(report_file),
        jobs = jobs,
        timeout = timeout_secs.saturating_mul(1000),
        temp = json_str(temp_dir),
    )
}

/// Parse a Stryker JSON report (mutation-testing report schema) into outcomes,
/// keeping only mutants whose line is in the changed set for their file.
fn parse_report(
    json: &str,
    allowed: &BTreeMap<String, BTreeSet<u32>>,
) -> Result<MutationResults> {
    let value: serde_json::Value = serde_json::from_str(json).context("invalid JSON")?;
    let files = value
        .get("files")
        .and_then(|f| f.as_object())
        .context("report has no `files` object")?;

    let mut results = MutationResults::default();
    for (report_path, file) in files {
        let Some((key, lines)) = match_allowed(report_path, allowed) else {
            continue; // a file outside the changed set
        };
        let Some(mutants) = file.get("mutants").and_then(|m| m.as_array()) else {
            continue;
        };
        for mutant in mutants {
            let line = mutant
                .get("location")
                .and_then(|l| l.get("start"))
                .and_then(|s| s.get("line"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as u32;
            if !lines.contains(&line) {
                continue; // outside the diff's changed lines
            }
            let status = mutant.get("status").and_then(|s| s.as_str()).unwrap_or("");
            let mutator = mutant
                .get("mutatorName")
                .and_then(|m| m.as_str())
                .unwrap_or("mutation");
            let column = mutant
                .get("location")
                .and_then(|l| l.get("start"))
                .and_then(|s| s.get("column"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as u32;
            match status {
                "Killed" => results.caught += 1,
                "Survived" | "NoCoverage" => {
                    results
                        .survivors
                        .push(make_mutant(key, line, column, mutator));
                }
                "Timeout" => results.timed_out += 1,
                // CompileError / RuntimeError / Ignored / unknown: didn't yield a
                // trustworthy result, so treat as unviable rather than caught.
                _ => results.unviable += 1,
            }
        }
    }
    Ok(results)
}

/// Match a report file path to a changed-file key, tolerating absolute paths
/// and `./` prefixes. Returns the canonical key and its changed-line set.
fn match_allowed<'a>(
    report_path: &str,
    allowed: &'a BTreeMap<String, BTreeSet<u32>>,
) -> Option<(&'a str, &'a BTreeSet<u32>)> {
    let norm = report_path.replace('\\', "/");
    let norm = norm.strip_prefix("./").unwrap_or(&norm);
    for (key, lines) in allowed {
        let k = key.as_str();
        if norm == k || norm.ends_with(&format!("/{k}")) || k.ends_with(&format!("/{norm}")) {
            return Some((k, lines));
        }
    }
    None
}

fn make_mutant(file: &str, line: u32, column: u32, mutator: &str) -> Mutant {
    Mutant {
        name: format!("{file}:{line}:{column}: {mutator}"),
        file: file.to_string(),
        line,
        column,
        function: None,
        description: mutator.to_string(),
    }
}

/// Minimal JSON string escaping for config values (paths, identifiers).
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::test_support::ScriptedRunner;

    fn allowed(pairs: &[(&str, &[u32])]) -> BTreeMap<String, BTreeSet<u32>> {
        pairs
            .iter()
            .map(|(p, ls)| (p.to_string(), ls.iter().copied().collect()))
            .collect()
    }

    fn sample_report() -> &'static str {
        r#"{
          "schemaVersion": "1.0",
          "files": {
            "src/calc.js": {
              "language": "javascript",
              "source": "…",
              "mutants": [
                { "id": "0", "mutatorName": "ArithmeticOperator", "status": "Survived",
                  "location": { "start": { "line": 2, "column": 10 }, "end": { "line": 2, "column": 11 } } },
                { "id": "1", "mutatorName": "EqualityOperator", "status": "Killed",
                  "location": { "start": { "line": 2, "column": 20 }, "end": { "line": 2, "column": 22 } } },
                { "id": "2", "mutatorName": "BlockStatement", "status": "Survived",
                  "location": { "start": { "line": 9, "column": 1 }, "end": { "line": 9, "column": 2 } } },
                { "id": "3", "mutatorName": "ConditionalExpression", "status": "Timeout",
                  "location": { "start": { "line": 2, "column": 30 }, "end": { "line": 2, "column": 31 } } }
              ]
            }
          }
        }"#
    }

    #[test]
    fn parses_and_buckets_by_status() {
        // Changed lines = {2} only; the line-9 survivor is out of scope.
        let r = parse_report(sample_report(), &allowed(&[("src/calc.js", &[2])])).unwrap();
        assert_eq!(r.survivors.len(), 1); // line-2 Survived
        assert_eq!(r.caught, 1); // line-2 Killed
        assert_eq!(r.timed_out, 1); // line-2 Timeout
        assert_eq!(r.survivors[0].line, 2);
        assert_eq!(r.survivors[0].description, "ArithmeticOperator");
    }

    #[test]
    fn includes_line_9_when_in_scope() {
        let r = parse_report(sample_report(), &allowed(&[("src/calc.js", &[2, 9])])).unwrap();
        assert_eq!(r.survivors.len(), 2); // both Survived mutants now counted
    }

    #[test]
    fn matches_absolute_report_paths() {
        let r = parse_report(
            &sample_report().replace("src/calc.js", "/work/repo/src/calc.js"),
            &allowed(&[("src/calc.js", &[2])]),
        )
        .unwrap();
        assert_eq!(r.caught, 1);
        assert_eq!(r.survivors.len(), 1);
    }

    #[test]
    fn config_has_runner_mutate_and_report() {
        let cfg = stryker_config(
            "mocha",
            &["src/a.js".to_string(), "src/b.ts".to_string()],
            "/tmp/out.json",
            4,
            60,
            "/tmp/.stryker",
        );
        assert!(cfg.contains("\"testRunner\": \"mocha\""));
        assert!(cfg.contains("\"src/a.js\""));
        assert!(cfg.contains("\"src/b.ts\""));
        assert!(cfg.contains("\"timeoutMS\": 60000"));
        assert!(cfg.contains("\"concurrency\": 4"));
        // perTest coverage analysis = run only the tests that cover each mutant.
        assert!(cfg.contains("\"coverageAnalysis\": \"perTest\""));
    }

    #[test]
    fn detect_runner_reads_package_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "devDependencies": { "@stryker-mutator/core": "8", "@stryker-mutator/mocha-runner": "8" } }"#,
        )
        .unwrap();
        assert_eq!(detect_runner(dir.path()).as_deref(), Some("mocha"));
    }

    #[test]
    fn detect_runner_none_without_plugin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{ "devDependencies": {} }"#).unwrap();
        assert_eq!(detect_runner(dir.path()), None);
    }

    #[test]
    fn is_available_true_when_probe_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let runner = ScriptedRunner::new();
        runner.push_ok("8.2.0"); // stryker --version
        assert!(is_available(&runner, dir.path()));
    }

    #[test]
    fn is_available_false_when_probe_fails() {
        let dir = tempfile::tempdir().unwrap();
        let runner = ScriptedRunner::new();
        runner.push_fail(127, "command not found: stryker");
        assert!(!is_available(&runner, dir.path()));
    }

    #[test]
    fn run_parses_report_and_buckets_outcomes() {
        // End-to-end through `run`: detect the runner from package.json, invoke
        // `stryker run` (scripted), then read+parse the JSON report we pre-write
        // to the work dir. Guards the whole function against being replaced with
        // a default return.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        std::fs::write(
            repo.join("package.json"),
            r#"{ "devDependencies": { "@stryker-mutator/mocha-runner": "8" } }"#,
        )
        .unwrap();
        // Pre-write the report `run` reads back after invoking Stryker.
        std::fs::write(repo.join("stryker-report.json"), sample_report()).unwrap();

        let cfg = Config {
            repo: repo.to_path_buf(),
            ..Config::default()
        };
        let changes = vec![FileChange {
            path: "src/calc.js".to_string(),
            added_lines: vec![2],
        }];

        let runner = ScriptedRunner::new();
        runner.push_ok("done"); // stryker run
        let results = run(&runner, &cfg, &changes, repo).unwrap();
        // Line-2 mutants only (line 9 is out of the changed set): 1 Survived,
        // 1 Killed, 1 Timeout.
        assert_eq!(results.survivors.len(), 1);
        assert_eq!(results.caught, 1);
        assert_eq!(results.timed_out, 1);
    }
}
