// SPDX-License-Identifier: Apache-2.0
//! Phase 4 (cont.) — Python mutation via **cosmic-ray** (advisory PoC).
//!
//! The Rust path drives `cargo-mutants`; this drives `cosmic-ray` for changed
//! `.py` files, then feeds the survivors through the *same* verdict, severity
//! ranking, and telemetry as Rust. The gate stays diff-scoped: cosmic-ray
//! mutates the changed files, and we keep only the mutants whose line falls on
//! the diff's changed lines (see [`crate::diff::PyFileChange`]).
//!
//! cosmic-ray is a session-based engine, so a run is four steps per file:
//! `init` (enumerate mutations into a sqlite session), `exec` (run them), and
//! `dump` (emit JSON-Lines results). The determinism pre-flight is handled by
//! the caller via the configured Python test command, so we skip cosmic-ray's
//! own `baseline`.
//!
//! Deferred (vs the Rust path): a per-function cap, pre-exec line filtering
//! (cosmic-ray execs the whole changed file; we filter after), Python
//! zero-assertion detection, and CI packaging of the Python toolchain.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::config::Config;
use crate::diff::PyFileChange;
use crate::mutants::MutationResults;
use crate::report::Mutant;
use crate::runner::CommandRunner;

/// Probe whether the cosmic-ray engine is installed (`cosmic-ray --version`).
/// Unlike `cargo-mutants`, cosmic-ray is a top-level command, so this is direct.
pub fn is_available(runner: &dyn CommandRunner, repo: &Path) -> bool {
    runner
        .run("cosmic-ray", &["--version"], repo)
        .map(|o| o.success)
        .unwrap_or(false)
}

/// Run cosmic-ray over every changed Python file, accumulating outcomes scoped
/// to the changed lines.
pub fn run(
    runner: &dyn CommandRunner,
    cfg: &Config,
    changes: &[PyFileChange],
    work_dir: &Path,
) -> Result<MutationResults> {
    let mut combined = MutationResults::default();
    for (idx, change) in changes.iter().enumerate() {
        let one = run_one_file(runner, cfg, change, work_dir, idx)?;
        combined.survivors.extend(one.survivors);
        combined.caught += one.caught;
        combined.timed_out += one.timed_out;
        combined.unviable += one.unviable;
    }
    Ok(combined)
}

/// init → exec → dump for one changed file, filtered to its changed lines.
fn run_one_file(
    runner: &dyn CommandRunner,
    cfg: &Config,
    change: &PyFileChange,
    work_dir: &Path,
    idx: usize,
) -> Result<MutationResults> {
    let cfg_path = work_dir.join(format!("cosmic-ray-{idx}.toml"));
    let session = work_dir.join(format!("cosmic-ray-{idx}.sqlite"));
    let test_command = cfg.python_test_command.join(" ");

    std::fs::write(
        &cfg_path,
        cosmic_ray_config(&change.path, cfg.timeout_secs, &test_command),
    )
    .with_context(|| format!("writing cosmic-ray config {}", cfg_path.display()))?;

    let cfg_s = cfg_path.to_string_lossy().into_owned();
    let sess_s = session.to_string_lossy().into_owned();

    let init = runner
        .run("cosmic-ray", &["init", &cfg_s, &sess_s], &cfg.repo)
        .context("running `cosmic-ray init`")?;
    if !init.success {
        bail!("cosmic-ray init failed:\n{}", init.combined());
    }
    // exec may exit non-zero in some configurations; the session dump is the
    // source of truth, so we don't gate on its exit code.
    runner
        .run("cosmic-ray", &["exec", &cfg_s, &sess_s], &cfg.repo)
        .context("running `cosmic-ray exec`")?;
    let dump = runner
        .run("cosmic-ray", &["dump", &sess_s], &cfg.repo)
        .context("running `cosmic-ray dump`")?;
    if !dump.success {
        bail!("cosmic-ray dump failed:\n{}", dump.combined());
    }

    let allowed: BTreeSet<u32> = change.added_lines.iter().copied().collect();
    parse_dump(&dump.stdout, &change.path, &allowed).context("parsing `cosmic-ray dump` output")
}

/// Build a minimal cosmic-ray TOML config for one module path.
fn cosmic_ray_config(module_path: &str, timeout_secs: u64, test_command: &str) -> String {
    format!(
        "[cosmic-ray]\n\
         module-path = \"{module}\"\n\
         timeout = {timeout}.0\n\
         excluded-modules = []\n\
         test-command = \"{test}\"\n\
         \n\
         [cosmic-ray.distributor]\n\
         name = \"local\"\n",
        module = toml_escape(module_path),
        timeout = timeout_secs,
        test = toml_escape(test_command),
    )
}

/// Escape a string for a basic (double-quoted) TOML value: backslash, quote,
/// the named control escapes, and any other control codepoint as `\uXXXX`.
fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0C}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Parse `cosmic-ray dump` (JSON-Lines; each line is `[work_item, result]`)
/// into [`MutationResults`], keeping only mutants whose line is in `allowed`.
fn parse_dump(stdout: &str, module_path: &str, allowed: &BTreeSet<u32>) -> Result<MutationResults> {
    let mut results = MutationResults::default();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(line).with_context(|| format!("invalid dump line: {line}"))?;
        let Some(arr) = value.as_array() else {
            continue;
        };
        // A not-yet-executed work item has no result element — skip it.
        let (Some(work_item), Some(result)) = (arr.first(), arr.get(1)) else {
            continue;
        };

        let Some(mutation) = work_item
            .get("mutations")
            .and_then(|m| m.as_array())
            .and_then(|a| a.first())
        else {
            continue;
        };
        let start = mutation.get("start_pos").and_then(|p| p.as_array());
        let line_no = start
            .and_then(|p| p.first())
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        // Diff scope: ignore mutants off the changed lines.
        if !allowed.contains(&line_no) {
            continue;
        }
        let column = start
            .and_then(|p| p.get(1))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let operator = mutation
            .get("operator_name")
            .and_then(|v| v.as_str())
            .unwrap_or("mutation");
        let function = mutation
            .get("definition_name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let worker = result
            .get("worker_outcome")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let test_outcome = result
            .get("test_outcome")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match (worker, test_outcome) {
            ("timeout", _) => results.timed_out += 1,
            (_, "survived") => results.survivors.push(make_mutant(
                module_path,
                line_no,
                column,
                operator,
                function,
            )),
            (_, "killed") => results.caught += 1,
            (_, "incompetent") => results.unviable += 1,
            // skipped / abnormal worker, or anything unrecognised: didn't yield
            // a trustworthy test result, so treat as unviable rather than caught.
            _ => results.unviable += 1,
        }
    }
    Ok(results)
}

/// Build a [`Mutant`] from a cosmic-ray mutation, humanising the operator name.
fn make_mutant(
    file: &str,
    line: u32,
    column: u32,
    operator: &str,
    function: Option<String>,
) -> Mutant {
    let description = operator
        .strip_prefix("core/")
        .unwrap_or(operator)
        .to_string();
    Mutant {
        name: format!("{file}:{line}:{column}: {description}"),
        file: file.to_string(),
        line,
        column,
        function,
        description,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One real `cosmic-ray dump` line per outcome, in the 8.4.x schema.
    fn survived_line() -> &'static str {
        r#"[{"job_id":"j1","mutations":[{"module_path":"adult.py","operator_name":"core/ReplaceComparisonOperator_GtE_Gt","occurrence":0,"start_pos":[2,15],"end_pos":[2,17],"operator_args":{},"definition_name":"is_adult"}]},{"worker_outcome":"normal","output":"","test_outcome":"survived","diff":""}]"#
    }
    fn killed_line() -> &'static str {
        r#"[{"job_id":"j2","mutations":[{"module_path":"adult.py","operator_name":"core/NumberReplacer","occurrence":0,"start_pos":[2,18],"end_pos":[2,20],"operator_args":{},"definition_name":"is_adult"}]},{"worker_outcome":"normal","output":"","test_outcome":"killed","diff":""}]"#
    }
    fn off_line() -> &'static str {
        // A survivor on line 9 — outside the changed set.
        r#"[{"job_id":"j3","mutations":[{"module_path":"adult.py","operator_name":"core/ReplaceComparisonOperator_GtE_Lt","occurrence":0,"start_pos":[9,4],"end_pos":[9,6],"operator_args":{},"definition_name":"other"}]},{"worker_outcome":"normal","output":"","test_outcome":"survived","diff":""}]"#
    }

    fn allowed(lines: &[u32]) -> BTreeSet<u32> {
        lines.iter().copied().collect()
    }

    #[test]
    fn parses_survivor_with_location_and_function() {
        let r = parse_dump(survived_line(), "adult.py", &allowed(&[2])).unwrap();
        assert_eq!(r.survivors.len(), 1);
        let m = &r.survivors[0];
        assert_eq!(m.file, "adult.py");
        assert_eq!(m.line, 2);
        assert_eq!(m.column, 15);
        assert_eq!(m.function.as_deref(), Some("is_adult"));
        // `core/` prefix stripped; comparison operator preserved for severity.
        assert_eq!(m.description, "ReplaceComparisonOperator_GtE_Gt");
    }

    #[test]
    fn counts_killed_as_caught() {
        let r = parse_dump(killed_line(), "adult.py", &allowed(&[2])).unwrap();
        assert_eq!(r.caught, 1);
        assert!(r.survivors.is_empty());
    }

    #[test]
    fn filters_mutants_off_the_changed_lines() {
        // Two survivors, one on line 2 (changed) and one on line 9 (not).
        let dump = format!("{}\n{}", survived_line(), off_line());
        let r = parse_dump(&dump, "adult.py", &allowed(&[2])).unwrap();
        assert_eq!(r.survivors.len(), 1);
        assert_eq!(r.survivors[0].line, 2);
    }

    #[test]
    fn tested_counts_completed_runs() {
        let dump = format!("{}\n{}", survived_line(), killed_line());
        let r = parse_dump(&dump, "adult.py", &allowed(&[2])).unwrap();
        assert_eq!(r.survivors.len(), 1);
        assert_eq!(r.caught, 1);
        assert_eq!(r.tested(), 2);
    }

    #[test]
    fn blank_and_unexecuted_lines_are_skipped() {
        let unexecuted = r#"[{"job_id":"j4","mutations":[{"module_path":"adult.py","operator_name":"core/NumberReplacer","start_pos":[2,1]}]}]"#;
        let dump = format!("\n{}\n{}\n", survived_line(), unexecuted);
        let r = parse_dump(&dump, "adult.py", &allowed(&[2])).unwrap();
        assert_eq!(r.survivors.len(), 1); // only the executed survivor
    }

    #[test]
    fn toml_escape_handles_quotes_backslashes_and_controls() {
        assert_eq!(toml_escape(r#"a\b"c"#), r#"a\\b\"c"#);
        assert_eq!(toml_escape("line1\nline2\t!"), "line1\\nline2\\t!");
        assert_eq!(toml_escape("\u{01}"), "\\u0001");
    }

    #[test]
    fn config_has_required_sections() {
        let toml = cosmic_ray_config("pkg/mod.py", 45, "python3 -m pytest -q");
        assert!(toml.contains("module-path = \"pkg/mod.py\""));
        assert!(toml.contains("timeout = 45.0"));
        assert!(toml.contains("test-command = \"python3 -m pytest -q\""));
        assert!(toml.contains("[cosmic-ray.distributor]"));
        assert!(toml.contains("name = \"local\""));
    }
}
