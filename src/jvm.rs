// SPDX-License-Identifier: Apache-2.0
//! Java/Kotlin mutation via **PIT** (pitest) — advisory PoC, Track A adapter.
//!
//! PIT is the most mature mutation engine on the JVM; it runs as a Maven goal
//! (`org.pitest:pitest-maven:mutationCoverage`) or a Gradle task (`pitest`),
//! mutates the configured classes, runs the suite, and writes a
//! `mutations.xml` report. Like the Python/JS adapters, PIT has no native
//! line-level diff scoping, so we run it and filter the XML report to the
//! diff's changed files + lines.
//!
//! Build tool is auto-detected: `pom.xml` → Maven, `build.gradle[.kts]` →
//! Gradle (preferring the `./gradlew` wrapper). A missing tool or an
//! unconfigured PIT plugin warns and skips rather than failing the gate.
//!
//! Deferred (vs the Rust path): per-function cap, Kotlin needs the
//! `pitest-kotlin` plugin in the build, JVM-aware severity, and CI packaging of
//! the JDK + build tool.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::Config;
use crate::diff::FileChange;
use crate::mutants::MutationResults;
use crate::report::Mutant;
use crate::runner::CommandRunner;

/// Which JVM build tool drives PIT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildTool {
    Maven,
    Gradle,
}

/// Detect the build tool from the repo root: Maven (`pom.xml`) is preferred,
/// then Gradle (`build.gradle` / `build.gradle.kts`). `None` if neither.
fn detect_build_tool(repo: &Path) -> Option<BuildTool> {
    if repo.join("pom.xml").is_file() {
        Some(BuildTool::Maven)
    } else if repo.join("build.gradle").is_file() || repo.join("build.gradle.kts").is_file() {
        Some(BuildTool::Gradle)
    } else {
        None
    }
}

/// Probe whether the detected build tool is runnable.
pub fn is_available(runner: &dyn CommandRunner, repo: &Path) -> bool {
    match detect_build_tool(repo) {
        Some(BuildTool::Maven) => probe(runner, repo, "mvn", &["-version"]),
        Some(BuildTool::Gradle) => {
            // Prefer the wrapper; fall back to a system gradle.
            (repo.join("gradlew").is_file() && probe(runner, repo, "./gradlew", &["-version"]))
                || probe(runner, repo, "gradle", &["-version"])
        }
        None => false,
    }
}

fn probe(runner: &dyn CommandRunner, repo: &Path, prog: &str, args: &[&str]) -> bool {
    runner.run(prog, args, repo).map(|o| o.success).unwrap_or(false)
}

/// Run PIT over the project, returning outcomes scoped to the changed files +
/// lines.
pub fn run(
    runner: &dyn CommandRunner,
    cfg: &Config,
    changes: &[FileChange],
    _work_dir: &Path,
) -> Result<MutationResults> {
    // Captured before PIT runs so we only accept a report this run produced —
    // a stale `mutations.xml` from a prior run (e.g. if this run errors before
    // emitting one) must not silently score the gate.
    let started_at = std::time::SystemTime::now();
    let tool = detect_build_tool(&cfg.repo).context("no pom.xml or build.gradle found")?;

    let out = match tool {
        BuildTool::Maven => runner
            .run(
                "mvn",
                &[
                    "-B",
                    "-q",
                    "org.pitest:pitest-maven:mutationCoverage",
                    "-DtimestampedReports=false",
                    "-DoutputFormats=XML",
                ],
                &cfg.repo,
            )
            .context("running Maven PIT goal")?,
        BuildTool::Gradle => {
            let gradlew = cfg.repo.join("gradlew");
            let prog = if gradlew.is_file() { "./gradlew" } else { "gradle" };
            runner
                .run(prog, &["pitest"], &cfg.repo)
                .context("running Gradle pitest task")?
        }
    };

    // PIT exits non-zero when its mutation threshold isn't met — that's a
    // signal, not an error. The XML report is the source of truth.
    let report = find_report(&cfg.repo, tool, started_at).ok_or_else(|| {
        anyhow::anyhow!(
            "PIT produced no fresh mutations.xml report (exit {:?}):\n{}",
            out.code,
            out.combined()
        )
    })?;
    let xml = std::fs::read_to_string(&report)
        .with_context(|| format!("reading PIT report {}", report.display()))?;

    let allowed = changes
        .iter()
        .map(|c| (c.path.clone(), c.added_lines.iter().copied().collect()))
        .collect();
    parse_report(&xml, &allowed).context("parsing PIT mutations.xml")
}

/// Locate the `mutations.xml` this run produced, newest first. `not_before` is
/// the run's start time; reports older than it are stale leftovers from a prior
/// run and are skipped so a failed run can't be scored against old data.
fn find_report(repo: &Path, tool: BuildTool, not_before: std::time::SystemTime) -> Option<PathBuf> {
    let root = match tool {
        BuildTool::Maven => repo.join("target/pit-reports"),
        BuildTool::Gradle => repo.join("build/reports/pitest"),
    };
    // Non-timestamped report sits directly under the root.
    let direct = root.join("mutations.xml");
    if direct.is_file() && is_fresh(&direct, not_before) {
        return Some(direct);
    }
    // Otherwise PIT wrote a timestamped subdir; pick the most recent that is
    // still fresh for this run.
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    collect_reports(&root, &mut found, 0);
    found.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    found
        .into_iter()
        .filter(|(mtime, _)| *mtime >= not_before)
        .map(|(_, p)| p)
        .next()
}

/// Whether `path`'s mtime is at or after `not_before` (i.e. written this run).
fn is_fresh(path: &Path, not_before: std::time::SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|m| m >= not_before)
        .unwrap_or(false)
}

fn collect_reports(dir: &Path, out: &mut Vec<(std::time::SystemTime, PathBuf)>, depth: usize) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_reports(&p, out, depth + 1);
        } else if p.file_name().and_then(|n| n.to_str()) == Some("mutations.xml") {
            let mtime = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            out.push((mtime, p));
        }
    }
}

/// Parse PIT's `mutations.xml` into outcomes, keeping only mutations whose line
/// is in the changed set for their file.
fn parse_report(xml: &str, allowed: &BTreeMap<String, BTreeSet<u32>>) -> Result<MutationResults> {
    let doc = roxmltree::Document::parse(xml).context("invalid XML")?;
    let mut results = MutationResults::default();

    for m in doc.descendants().filter(|n| n.has_tag_name("mutation")) {
        let status = m.attribute("status").unwrap_or("");
        let source = child_text(m, "sourceFile").unwrap_or_default();
        let class = child_text(m, "mutatedClass").unwrap_or_default();
        let line = child_text(m, "lineNumber")
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);

        let Some((key, lines)) = match_allowed(&source, &class, allowed) else {
            continue;
        };
        if !lines.contains(&line) {
            continue;
        }

        let mutator = child_text(m, "mutator").unwrap_or_default();
        let method = child_text(m, "mutatedMethod").unwrap_or_default();
        match status {
            "KILLED" => results.caught += 1,
            "SURVIVED" | "NO_COVERAGE" => {
                results
                    .survivors
                    .push(make_mutant(key, line, &mutator, &method));
            }
            "TIMED_OUT" => results.timed_out += 1,
            // MEMORY_ERROR / RUN_ERROR / NON_VIABLE / unknown: no trustworthy
            // result → unviable.
            _ => results.unviable += 1,
        }
    }
    Ok(results)
}

/// Text of the first child element with `tag`.
fn child_text<'a>(node: roxmltree::Node<'a, 'a>, tag: &str) -> Option<String> {
    node.children()
        .find(|c| c.has_tag_name(tag))
        .and_then(|c| c.text())
        .map(|t| t.trim().to_string())
}

/// Match PIT's `sourceFile` (a bare file name like `Calc.java`) to a changed-
/// file key. PIT also records `mutatedClass` (e.g. `com.example.Calc`), whose
/// package qualifies the file: we first try a package-aware path suffix
/// (`com/example/Calc.java`) so identical basenames in different packages don't
/// collide, then fall back to basename matching when there is no package info.
fn match_allowed<'a>(
    source: &str,
    mutated_class: &str,
    allowed: &'a BTreeMap<String, BTreeSet<u32>>,
) -> Option<(&'a str, &'a BTreeSet<u32>)> {
    if source.is_empty() {
        return None;
    }
    // Prefer package-qualified matching to disambiguate duplicate file names.
    if let Some(pkg) = package_path(mutated_class) {
        let suffix = format!("{pkg}/{source}");
        for (key, lines) in allowed {
            let k = key.replace('\\', "/");
            if k == suffix || k.ends_with(&format!("/{suffix}")) {
                return Some((key, lines));
            }
        }
    }
    // Fallback: basename match (default package, or no package-qualified hit).
    for (key, lines) in allowed {
        let base = key.rsplit(['/', '\\']).next().unwrap_or(key);
        if base == source {
            return Some((key, lines));
        }
    }
    None
}

/// Package path from a PIT `mutatedClass` (`com.example.Calc` → `com/example`),
/// or `None` for the default package / empty input. The final `.`-segment is
/// the class name (possibly with a `$Nested` suffix) and is dropped.
fn package_path(mutated_class: &str) -> Option<String> {
    let (pkg, _class) = mutated_class.rsplit_once('.')?;
    if pkg.is_empty() {
        None
    } else {
        Some(pkg.replace('.', "/"))
    }
}

fn make_mutant(file: &str, line: u32, mutator: &str, method: &str) -> Mutant {
    // Humanise: last path segment of the mutator class, plus the method.
    let short = mutator.rsplit('.').next().unwrap_or(mutator);
    let description = if method.is_empty() {
        short.to_string()
    } else {
        format!("{short} in {method}")
    };
    Mutant {
        name: format!("{file}:{line}:0: {description}"),
        file: file.to_string(),
        line,
        column: 0,
        function: (!method.is_empty()).then(|| method.to_string()),
        description,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(pairs: &[(&str, &[u32])]) -> BTreeMap<String, BTreeSet<u32>> {
        pairs
            .iter()
            .map(|(p, ls)| (p.to_string(), ls.iter().copied().collect()))
            .collect()
    }

    fn sample() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <mutations>
          <mutation detected='true' status='KILLED'>
            <sourceFile>Calc.java</sourceFile>
            <mutatedClass>com.example.Calc</mutatedClass>
            <mutatedMethod>add</mutatedMethod>
            <lineNumber>12</lineNumber>
            <mutator>org.pitest.mutationtest.engine.gregor.mutators.MathMutator</mutator>
          </mutation>
          <mutation detected='false' status='SURVIVED'>
            <sourceFile>Calc.java</sourceFile>
            <mutatedClass>com.example.Calc</mutatedClass>
            <mutatedMethod>cmp</mutatedMethod>
            <lineNumber>12</lineNumber>
            <mutator>org.pitest...ConditionalsBoundaryMutator</mutator>
          </mutation>
          <mutation detected='false' status='NO_COVERAGE'>
            <sourceFile>Calc.java</sourceFile>
            <lineNumber>40</lineNumber>
            <mutator>org.pitest...VoidMethodCallMutator</mutator>
          </mutation>
          <mutation detected='false' status='TIMED_OUT'>
            <sourceFile>Calc.java</sourceFile>
            <lineNumber>12</lineNumber>
            <mutator>X</mutator>
          </mutation>
        </mutations>"#
    }

    #[test]
    fn parses_and_scopes_to_lines() {
        // changed lines = {12}; the line-40 NO_COVERAGE is out of scope.
        let r = parse_report(sample(), &allowed(&[("src/main/java/com/example/Calc.java", &[12])]))
            .unwrap();
        assert_eq!(r.caught, 1); // KILLED
        assert_eq!(r.survivors.len(), 1); // SURVIVED on line 12
        assert_eq!(r.timed_out, 1); // TIMED_OUT on line 12
        assert_eq!(r.survivors[0].line, 12);
        assert_eq!(r.survivors[0].function.as_deref(), Some("cmp"));
    }

    #[test]
    fn no_coverage_counts_when_in_scope() {
        let r = parse_report(sample(), &allowed(&[("Calc.java", &[12, 40])])).unwrap();
        assert_eq!(r.survivors.len(), 2); // SURVIVED(12) + NO_COVERAGE(40)
    }

    #[test]
    fn matches_by_basename() {
        // PIT's bare `Calc.java` matches a repo-relative path by basename.
        let r = parse_report(sample(), &allowed(&[("a/b/Calc.java", &[12])])).unwrap();
        assert_eq!(r.caught, 1);
    }

    #[test]
    fn class_aware_matching_disambiguates_duplicate_basenames() {
        // Two changed files share the basename `Calc.java` in different packages.
        // The mutation carries `mutatedClass=com.example.Calc`, so it must score
        // against the com/example file — even though `com/aaa` sorts first and
        // would win a basename-only match.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <mutations>
          <mutation detected='false' status='SURVIVED'>
            <sourceFile>Calc.java</sourceFile>
            <mutatedClass>com.example.Calc</mutatedClass>
            <mutatedMethod>add</mutatedMethod>
            <lineNumber>5</lineNumber>
            <mutator>org.pitest...MathMutator</mutator>
          </mutation>
        </mutations>"#;
        let r = parse_report(
            xml,
            &allowed(&[
                ("src/main/java/com/aaa/Calc.java", &[5]),
                ("src/main/java/com/example/Calc.java", &[5]),
            ]),
        )
        .unwrap();
        assert_eq!(r.survivors.len(), 1);
        assert_eq!(
            r.survivors[0].file, "src/main/java/com/example/Calc.java",
            "the mutatedClass package must disambiguate the duplicate basename"
        );
    }

    #[test]
    fn find_report_ignores_stale_report() {
        // A `mutations.xml` left from a previous run (older than this run's start)
        // must not be picked up.
        let dir = tempfile::tempdir().unwrap();
        let reports = dir.path().join("target/pit-reports");
        std::fs::create_dir_all(&reports).unwrap();
        let xml = reports.join("mutations.xml");
        std::fs::write(&xml, "<mutations/>").unwrap();
        let mtime = std::fs::metadata(&xml).unwrap().modified().unwrap();
        // Cutoff strictly after the file's mtime → the report predates this run.
        let cutoff = mtime + std::time::Duration::from_secs(10);
        assert!(find_report(dir.path(), BuildTool::Maven, cutoff).is_none());
    }

    #[test]
    fn find_report_accepts_fresh_report() {
        let dir = tempfile::tempdir().unwrap();
        let reports = dir.path().join("target/pit-reports");
        std::fs::create_dir_all(&reports).unwrap();
        let xml = reports.join("mutations.xml");
        std::fs::write(&xml, "<mutations/>").unwrap();
        let mtime = std::fs::metadata(&xml).unwrap().modified().unwrap();
        // Cutoff before the file's mtime → the report is from this run.
        let cutoff = mtime - std::time::Duration::from_secs(10);
        assert_eq!(find_report(dir.path(), BuildTool::Maven, cutoff), Some(xml));
    }

    #[test]
    fn unrelated_file_is_ignored() {
        let r = parse_report(sample(), &allowed(&[("Other.java", &[12])])).unwrap();
        assert!(r.survivors.is_empty());
        assert_eq!(r.caught, 0);
    }

    #[test]
    fn detect_build_tool_prefers_maven() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pom.xml"), "<project/>").unwrap();
        std::fs::write(dir.path().join("build.gradle"), "").unwrap();
        assert_eq!(detect_build_tool(dir.path()), Some(BuildTool::Maven));

        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(dir2.path().join("build.gradle.kts"), "").unwrap();
        assert_eq!(detect_build_tool(dir2.path()), Some(BuildTool::Gradle));

        assert_eq!(detect_build_tool(tempfile::tempdir().unwrap().path()), None);
    }
}
