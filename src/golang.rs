// SPDX-License-Identifier: Apache-2.0
//! Go mutation via **gremlins** (advisory PoC) — Track A language adapter.
//!
//! Mirrors the Python/JS adapters: run the engine, then feed survivors through
//! the *same* verdict, severity and telemetry as Rust. gremlins mutates the Go
//! module and runs `go test`; it has no native line-level diff scoping, so we
//! run it and filter the JSON report back to the diff's changed files + lines —
//! the same post-hoc tradeoff cosmic-ray and Stryker make.
//!
//! gremlins' compile-and-test cycle is fast (Go builds quickly), which suits the
//! COGS model well. Deferred (vs the Rust path): a per-function cap, package-
//! scoped runs (currently whole-module), Go zero-assertion + AST severity, and
//! CI packaging of the Go toolchain. Requires `gremlins` on `PATH`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::diff::FileChange;
use crate::mutants::MutationResults;
use crate::report::Mutant;
use crate::runner::CommandRunner;

/// Probe whether gremlins is installed (`gremlins --version`).
pub fn is_available(runner: &dyn CommandRunner, repo: &Path) -> bool {
    runner
        .run("gremlins", &["--version"], repo)
        .map(|o| o.success)
        .unwrap_or(false)
}

/// Run gremlins over the Go module, returning outcomes scoped to the changed
/// files + lines.
pub fn run(
    runner: &dyn CommandRunner,
    cfg: &Config,
    changes: &[FileChange],
    work_dir: &Path,
) -> Result<MutationResults> {
    let report_path = work_dir.join("gremlins-report.json");
    let report_s = report_path.to_string_lossy().into_owned();

    // gremlins exits non-zero when survivors push the score below threshold —
    // a signal, not an error. The JSON report is the source of truth.
    let out = runner
        .run(
            "gremlins",
            &["unleash", "./...", "--output", &report_s],
            &cfg.repo,
        )
        .context("running `gremlins unleash`")?;

    let json = std::fs::read_to_string(&report_path).map_err(|_| {
        anyhow::anyhow!(
            "gremlins produced no JSON report (exit {:?}):\n{}",
            out.code,
            out.combined()
        )
    })?;

    let allowed = changes
        .iter()
        .map(|c| (c.path.clone(), c.added_lines.iter().copied().collect()))
        .collect();
    parse_report(&json, &allowed).context("parsing gremlins JSON report")
}

/// Parse a gremlins JSON report into outcomes, keeping only mutants whose line
/// is in the changed set for their file. Tolerant of the schema's two shapes:
/// a nested `files[].mutations[]` and a flat `mutants[]`/`mutations[]`.
fn parse_report(json: &str, allowed: &BTreeMap<String, BTreeSet<u32>>) -> Result<MutationResults> {
    let value: serde_json::Value = serde_json::from_str(json).context("invalid JSON")?;
    let mut results = MutationResults::default();

    // Shape A: { "files": [ { filename, mutations: [ {..} ] } ] }
    if let Some(files) = value.get("files").and_then(|f| f.as_array()) {
        for file in files {
            let fname = str_field(file, &["filename", "file_name", "file", "name"]);
            let muts = file
                .get("mutations")
                .or_else(|| file.get("mutants"))
                .and_then(|m| m.as_array());
            if let (Some(fname), Some(muts)) = (fname, muts) {
                for m in muts {
                    record_mutant(m, Some(fname.as_str()), allowed, &mut results);
                }
            }
        }
    }

    // Shape B: flat array, each mutant carries its own file path.
    if let Some(muts) = value
        .get("mutants")
        .or_else(|| value.get("mutations"))
        .and_then(|m| m.as_array())
    {
        for m in muts {
            record_mutant(m, None, allowed, &mut results);
        }
    }

    Ok(results)
}

/// Bucket one mutant into `results` if its line is within the changed set.
/// `parent_file` is the enclosing file path (shape A); otherwise the path is
/// read from the mutant itself (shape B).
fn record_mutant(
    m: &serde_json::Value,
    parent_file: Option<&str>,
    allowed: &BTreeMap<String, BTreeSet<u32>>,
    results: &mut MutationResults,
) {
    let file = parent_file
        .map(str::to_string)
        .or_else(|| str_field(m, &["filename", "file_name", "file"]));
    let Some(file) = file else { return };
    let line = u32_field(m, &["line"]).or_else(|| {
        m.get("position")
            .or_else(|| m.get("location").and_then(|l| l.get("start")))
            .and_then(|p| u32_field(p, &["line"]))
    });
    let Some(line) = line else { return };

    let Some((key, lines)) = match_allowed(&file, allowed) else {
        return;
    };
    if !lines.contains(&line) {
        return;
    }

    let kind = str_field(m, &["type", "mutation_type", "mutator", "mutator_name"])
        .unwrap_or_else(|| "mutation".to_string());
    let column = u32_field(m, &["column"]).unwrap_or(0);
    let status = str_field(m, &["status", "result"]).unwrap_or_default();
    match normalize_status(&status).as_str() {
        "KILLED" => results.caught += 1,
        "LIVED" | "SURVIVED" | "NOTCOVERED" => {
            results
                .survivors
                .push(make_mutant(key, line, column, &kind));
        }
        "TIMEDOUT" => results.timed_out += 1,
        // NOTVIABLE / RUNERROR / unknown: no trustworthy result → unviable.
        _ => results.unviable += 1,
    }
}

/// Uppercase and strip spaces/underscores/hyphens so `"NOT COVERED"`,
/// `"not_covered"` and `"NotCovered"` all compare equal.
fn normalize_status(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, ' ' | '_' | '-'))
        .flat_map(char::to_uppercase)
        .collect()
}

/// Match a report file path to a changed-file key, tolerating absolute paths and
/// `./` prefixes. Returns the canonical key and its changed-line set.
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

fn str_field(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(|x| x.as_str()))
        .map(str::to_string)
}

fn u32_field(v: &serde_json::Value, keys: &[&str]) -> Option<u32> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(|x| x.as_u64()))
        .map(|n| n as u32)
}

fn make_mutant(file: &str, line: u32, column: u32, kind: &str) -> Mutant {
    Mutant {
        name: format!("{file}:{line}:{column}: {kind}"),
        file: file.to_string(),
        line,
        column,
        function: None,
        description: kind.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::runner::test_support::ScriptedRunner;
    use tempfile::tempdir;

    #[test]
    fn is_available_true_when_probe_succeeds() {
        let work = tempdir().unwrap();
        let runner = ScriptedRunner::new();
        runner.push_ok("gremlins version 0.5.0");
        assert!(is_available(&runner, work.path()));
    }

    #[test]
    fn is_available_false_when_probe_fails() {
        // Guards both `is_available -> true` and `is_available -> false` mutations.
        let work = tempdir().unwrap();
        let runner = ScriptedRunner::new();
        runner.push_fail(127, "command not found: gremlins");
        assert!(!is_available(&runner, work.path()));
    }

    #[test]
    fn run_parses_report_and_buckets_outcomes() {
        // Drive run() end-to-end: script gremlins unleash and pre-write the JSON
        // report it would produce. Kills `run -> Ok(Default::default())`.
        let work = tempdir().unwrap();
        std::fs::write(
            work.path().join("gremlins-report.json"),
            r#"{
              "files": [
                {
                  "filename": "calc.go",
                  "mutations": [
                    { "type": "CONDITIONALS_BOUNDARY", "status": "LIVED", "line": 3, "column": 1 },
                    { "type": "ARITHMETIC_BASE", "status": "KILLED", "line": 3, "column": 5 }
                  ]
                }
              ]
            }"#,
        )
        .unwrap();

        let cfg = Config {
            repo: work.path().to_path_buf(),
            ..Config::default()
        };
        let changes = vec![FileChange {
            path: "calc.go".to_string(),
            added_lines: vec![3],
        }];
        let runner = ScriptedRunner::new();
        runner.push_ok("done"); // gremlins unleash ./...

        let results = run(&runner, &cfg, &changes, work.path()).unwrap();
        assert_eq!(results.survivors.len(), 1);
        assert_eq!(results.caught, 1);
    }

    fn allowed(pairs: &[(&str, &[u32])]) -> BTreeMap<String, BTreeSet<u32>> {
        pairs
            .iter()
            .map(|(p, ls)| (p.to_string(), ls.iter().copied().collect()))
            .collect()
    }

    fn nested_report() -> &'static str {
        r#"{
          "go_module": "example.com/foo",
          "files": [
            {
              "filename": "calc.go",
              "mutations": [
                { "type": "CONDITIONALS_BOUNDARY", "status": "LIVED", "line": 5, "column": 9 },
                { "type": "ARITHMETIC_BASE", "status": "KILLED", "line": 5, "column": 20 },
                { "type": "INVERT_NEGATIVES", "status": "NOT COVERED", "line": 12, "column": 1 },
                { "type": "CONDITIONALS_NEGATION", "status": "TIMED OUT", "line": 5, "column": 30 }
              ]
            }
          ]
        }"#
    }

    #[test]
    fn nested_shape_buckets_by_status_scoped_to_lines() {
        // Changed lines = {5} only; the line-12 NOT COVERED is out of scope.
        let r = parse_report(nested_report(), &allowed(&[("calc.go", &[5])])).unwrap();
        assert_eq!(r.survivors.len(), 1); // line-5 LIVED
        assert_eq!(r.caught, 1); // line-5 KILLED
        assert_eq!(r.timed_out, 1); // line-5 TIMED OUT
        assert_eq!(r.survivors[0].line, 5);
        assert_eq!(r.survivors[0].description, "CONDITIONALS_BOUNDARY");
    }

    #[test]
    fn not_covered_counts_as_survivor_when_in_scope() {
        let r = parse_report(nested_report(), &allowed(&[("calc.go", &[5, 12])])).unwrap();
        assert_eq!(r.survivors.len(), 2); // LIVED (5) + NOT COVERED (12)
    }

    #[test]
    fn flat_shape_is_supported() {
        let flat = r#"{
          "mutants": [
            { "file": "pkg/a.go", "line": 3, "status": "lived", "mutator": "X" },
            { "file": "pkg/a.go", "line": 3, "status": "killed", "mutator": "Y" }
          ]
        }"#;
        let r = parse_report(flat, &allowed(&[("pkg/a.go", &[3])])).unwrap();
        assert_eq!(r.survivors.len(), 1);
        assert_eq!(r.caught, 1);
    }

    #[test]
    fn matches_absolute_paths() {
        let r = parse_report(
            &nested_report().replace("calc.go", "/work/repo/calc.go"),
            &allowed(&[("calc.go", &[5])]),
        )
        .unwrap();
        assert_eq!(r.caught, 1);
        assert_eq!(r.survivors.len(), 1);
    }

    #[test]
    fn status_normalization_is_robust() {
        assert_eq!(normalize_status("NOT COVERED"), "NOTCOVERED");
        assert_eq!(normalize_status("not_viable"), "NOTVIABLE");
        assert_eq!(normalize_status("TimedOut"), "TIMEDOUT");
    }

    #[test]
    fn unviable_and_unknown_do_not_become_survivors() {
        let j = r#"{ "mutants": [
            { "file": "a.go", "line": 1, "status": "NOT VIABLE", "type": "T" },
            { "file": "a.go", "line": 1, "status": "RUNERROR", "type": "T" },
            { "file": "a.go", "line": 1, "status": "wat", "type": "T" }
        ]}"#;
        let r = parse_report(j, &allowed(&[("a.go", &[1])])).unwrap();
        assert!(r.survivors.is_empty());
        assert_eq!(r.unviable, 3);
    }
}
