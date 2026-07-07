// SPDX-License-Identifier: Apache-2.0
//! Pattern lane: documentation-standard compliance (Track B).
//!
//! Enforces the documentation flow after implementation: when a PR changes a
//! module's *code*, that module's docs must still satisfy the documentation
//! standard (see the monorepo's `docs/DOCUMENTATION_STANDARD.md`) — and the
//! reference docs must move *with* the code they describe. Two kinds of checks:
//!
//! 1. **Presence** — the touched module has a README with a quickstart, an
//!    as-built architecture diagram (mermaid `flowchart`/`graph`), and an
//!    event/call flow (`sequenceDiagram`), in README.md or ARCHITECTURE.md
//!    (root or `docs/`).
//! 2. **Drift** — the diff adds configuration surface (clap args, env reads)
//!    or HTTP surface (router routes) in a module without touching any doc
//!    file in that module. Docs that don't move with the code are how every
//!    fictional-demo/dead-flag doc in this repo happened.
//!
//! A **module** is the nearest ancestor of a changed source file that holds a
//! `README.md` next to a manifest (`Cargo.toml`/`package.json`/`pyproject.toml`
//! /`go.mod`), falling back to the repo root. Purely static and diff-scoped,
//! like every pattern lane: advisory by default, gates only via
//! `block_on_pattern: ["docs" | <rule-id> | "all"]`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::pattern::{PatternFinding, PatternReport};

/// Rule ids this lane can emit (each usable in `block_on_pattern`).
pub const RULES: [&str; 6] = [
    "docs-missing-readme",
    "docs-missing-architecture",
    "docs-missing-event-flow",
    "docs-missing-quickstart",
    "docs-stale-config",
    "docs-stale-api",
];

/// File names (relative to a module root) that count as that module's docs.
const DOC_MARKERS: [&str; 6] = [
    "README.md",
    "ARCHITECTURE.md",
    "docs/ARCHITECTURE.md",
    "docs/CONFIG.md",
    "docs/API.md",
    "docs/OPERATIONS.md",
];

/// Scan the diff for documentation-standard violations. `diff_text` is the
/// unified diff of the whole PR (the lane derives changed paths and added
/// lines from it directly, so it sees every language, not just Rust).
pub fn scan(repo: &Path, diff_text: &str) -> PatternReport {
    let changed = changed_paths(diff_text);
    if changed.is_empty() {
        return PatternReport::default();
    }

    // Group the changed files by module root; remember whether each module had
    // code changes, doc changes, and which added lines belong to it.
    let mut modules: BTreeMap<PathBuf, ModuleChanges> = BTreeMap::new();
    for file in &changed {
        let root = module_root(repo, file);
        let entry = modules.entry(root).or_default();
        if is_doc_file(file) {
            entry.docs_touched = true;
        } else if is_source_file(file) {
            entry.code_files.push(file.clone());
        }
    }

    let added_by_file = added_lines_by_file(diff_text);
    let mut findings = Vec::new();

    for (root, ch) in &modules {
        if ch.code_files.is_empty() {
            continue; // docs-only (or asset-only) change in this module: nothing to enforce
        }
        let rel_root = root
            .strip_prefix(repo)
            .unwrap_or(root)
            .to_string_lossy()
            .replace('\\', "/");
        let label = if rel_root.is_empty() { "." } else { &rel_root };
        let anchor = ch.code_files[0].clone();

        // ── presence checks ──────────────────────────────────────────────────
        let readme = root.join("README.md");
        if !readme.exists() {
            findings.push(finding(
                "docs-missing-readme",
                &anchor,
                format!("module {label} has code changes but no README.md"),
                40,
            ));
            continue; // the rest of the presence checks presuppose docs to inspect
        }
        let corpus = doc_corpus(root);
        if !has_architecture_diagram(&corpus) {
            findings.push(finding(
                "docs-missing-architecture",
                &anchor,
                format!(
                    "module {label} has no mermaid architecture diagram (flowchart) in \
                     README.md or ARCHITECTURE.md — required by the documentation standard"
                ),
                25,
            ));
        }
        if !has_event_flow(&corpus) {
            findings.push(finding(
                "docs-missing-event-flow",
                &anchor,
                format!(
                    "module {label} has no event/call flow (mermaid sequenceDiagram) — \
                     required by the documentation standard"
                ),
                15,
            ));
        }
        if !has_quickstart(&std::fs::read_to_string(&readme).unwrap_or_default()) {
            findings.push(finding(
                "docs-missing-quickstart",
                &anchor,
                format!("module {label} README has no Quickstart/Getting started section"),
                10,
            ));
        }

        // ── drift checks: code surface moved, docs did not ──────────────────
        if ch.docs_touched {
            continue; // docs moved with the code — the human reviewer judges content
        }
        let added: Vec<&str> = ch
            .code_files
            .iter()
            .filter_map(|f| added_by_file.get(f))
            .flatten()
            .map(String::as_str)
            .collect();
        if let Some(line) = added.iter().find(|l| is_config_surface(l)) {
            findings.push(finding(
                "docs-stale-config",
                &anchor,
                format!(
                    "module {label} adds configuration surface ({}) but no doc file in the \
                     module was touched — update docs/CONFIG.md (or the README config section)",
                    snippet(line)
                ),
                15,
            ));
        }
        if let Some(line) = added.iter().find(|l| is_api_surface(l)) {
            findings.push(finding(
                "docs-stale-api",
                &anchor,
                format!(
                    "module {label} adds HTTP surface ({}) but no doc file in the module \
                     was touched — update docs/API.md (or the README endpoints section)",
                    snippet(line)
                ),
                15,
            ));
        }
    }

    PatternReport::from_findings(findings)
}

#[derive(Default)]
struct ModuleChanges {
    code_files: Vec<String>,
    docs_touched: bool,
}

fn finding(rule: &str, file: &str, message: String, weight: u32) -> PatternFinding {
    PatternFinding {
        rule: rule.to_string(),
        file: file.to_string(),
        line: 0, // module-level findings — there is no single offending line
        message,
        weight,
    }
}

/// Changed file paths out of the unified diff (`+++ b/<path>` headers;
/// deletions have `+++ /dev/null` and are skipped — deleted code needs no docs).
fn changed_paths(diff_text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in diff_text.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            let path = path.trim();
            if !path.is_empty() && path != "/dev/null" {
                out.push(path.to_string());
            }
        }
    }
    out
}

/// Added lines per file, keyed by the `+++ b/` path they appeared under.
fn added_lines_by_file(diff_text: &str) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in diff_text.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current = Some(path.trim().to_string());
        } else if let (Some(file), Some(added)) = (&current, line.strip_prefix('+')) {
            if !line.starts_with("+++") {
                out.entry(file.clone()).or_default().push(added.to_string());
            }
        }
    }
    out
}

/// The nearest ancestor directory of `changed_file` (a repo-relative path) that
/// looks like a module root: README.md next to a project manifest. Falls back
/// to the repo root.
fn module_root(repo: &Path, changed_file: &str) -> PathBuf {
    const MANIFESTS: [&str; 4] = ["Cargo.toml", "package.json", "pyproject.toml", "go.mod"];
    let mut dir = repo.join(changed_file);
    dir.pop();
    while dir.starts_with(repo) {
        if dir.join("README.md").exists() && MANIFESTS.iter().any(|m| dir.join(m).exists()) {
            return dir;
        }
        // A manifest without a README is still the module boundary — the
        // missing README is then the finding, not a reason to walk past it.
        if MANIFESTS.iter().any(|m| dir.join(m).exists()) && dir != repo {
            return dir;
        }
        if dir == repo {
            break;
        }
        dir.pop();
    }
    repo.to_path_buf()
}

pub(crate) fn is_doc_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md") || lower.contains("/docs/") || lower.ends_with("openapi.yaml")
}

/// Code the standard cares about: application source, excluding tests and
/// generated/vendored trees.
fn is_source_file(path: &str) -> bool {
    let lower = path.replace('\\', "/").to_ascii_lowercase();
    let code_ext = [".rs", ".ts", ".tsx", ".js", ".py", ".go", ".java", ".kt"]
        .iter()
        .any(|e| lower.ends_with(e));
    if !code_ext {
        return false;
    }
    let excluded = [
        "/tests/",
        "/test/",
        "/target/",
        "/node_modules/",
        "/vendor/",
        "/dist/",
    ];
    !excluded.iter().any(|e| lower.contains(e)) && !lower.ends_with("_test.go")
}

/// Every doc file of the module, concatenated, for presence checks.
fn doc_corpus(root: &Path) -> String {
    DOC_MARKERS
        .iter()
        .filter_map(|rel| std::fs::read_to_string(root.join(rel)).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of every ```mermaid fenced block in the corpus — scoping the diagram
/// checks to fence contents so unrelated prose (e.g. the word "graph" in a sentence,
/// or a non-mermaid code block) can't satisfy them.
fn mermaid_blocks(corpus: &str) -> impl Iterator<Item = &str> {
    corpus
        .split("```mermaid")
        .skip(1)
        .filter_map(|s| s.split("```").next())
}

fn has_architecture_diagram(corpus: &str) -> bool {
    mermaid_blocks(corpus).any(|b| b.contains("flowchart") || b.contains("graph "))
}

fn has_event_flow(corpus: &str) -> bool {
    mermaid_blocks(corpus).any(|b| b.contains("sequenceDiagram"))
}

fn has_quickstart(readme: &str) -> bool {
    readme.lines().any(|l| {
        let l = l.trim_start_matches('#').trim().to_ascii_lowercase();
        l.starts_with("quickstart")
            || l.starts_with("quick start")
            || l.starts_with("getting started")
            || l.starts_with("try it")
    })
}

/// Added line introduces configuration surface: a clap arg, an env read, or a
/// serde default on a config field.
fn is_config_surface(line: &str) -> bool {
    let l = line.trim_start();
    l.contains("#[arg(")
        || l.contains("env::var(")
        || l.contains("std::env::var(")
        || l.contains("#[clap(")
        || (l.contains("#[serde(default") && !l.starts_with("//"))
}

/// Added line introduces HTTP surface (axum/actix/warp route registration).
fn is_api_surface(line: &str) -> bool {
    let l = line.trim_start();
    (l.contains(".route(\"") || l.contains("#[get(\"") || l.contains("#[post(\""))
        && !l.starts_with("//")
}

fn snippet(line: &str) -> String {
    let t = line.trim();
    let mut s: String = t.chars().take(60).collect();
    if t.chars().count() > 60 {
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repo with one documented module and one bare module.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let documented = dir.path().join("mods/good");
        std::fs::create_dir_all(documented.join("src")).unwrap();
        std::fs::write(documented.join("Cargo.toml"), "[package]\nname=\"good\"").unwrap();
        std::fs::write(
            documented.join("README.md"),
            "# good\n\n## Quickstart\n\nrun it\n\n## Architecture\n\n```mermaid\nflowchart TD\n a-->b\n```\n\n```mermaid\nsequenceDiagram\n a->>b: hi\n```\n",
        )
        .unwrap();
        let bare = dir.path().join("mods/bare");
        std::fs::create_dir_all(bare.join("src")).unwrap();
        std::fs::write(bare.join("Cargo.toml"), "[package]\nname=\"bare\"").unwrap();
        dir
    }

    fn diff_for(file: &str, added: &[&str]) -> String {
        let mut d = format!("diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n@@\n");
        for a in added {
            d.push_str(&format!("+{a}\n"));
        }
        d
    }

    #[test]
    fn documented_module_with_plain_code_change_is_clean() {
        let repo = fixture();
        let diff = diff_for("mods/good/src/lib.rs", &["fn helper() {}"]);
        let report = scan(repo.path(), &diff);
        assert!(
            report.findings.is_empty(),
            "unexpected findings: {:?}",
            report.findings
        );
    }

    #[test]
    fn bare_module_flags_missing_readme_only() {
        let repo = fixture();
        let diff = diff_for("mods/bare/src/lib.rs", &["fn f() {}"]);
        let report = scan(repo.path(), &diff);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].rule, "docs-missing-readme");
    }

    #[test]
    fn missing_diagrams_and_quickstart_are_each_flagged() {
        let repo = fixture();
        // Strip the good module's README down to prose only.
        std::fs::write(repo.path().join("mods/good/README.md"), "# good\nwords\n").unwrap();
        let diff = diff_for("mods/good/src/lib.rs", &["fn f() {}"]);
        let report = scan(repo.path(), &diff);
        let rules: Vec<&str> = report.findings.iter().map(|f| f.rule.as_str()).collect();
        assert!(rules.contains(&"docs-missing-architecture"));
        assert!(rules.contains(&"docs-missing-event-flow"));
        assert!(rules.contains(&"docs-missing-quickstart"));
    }

    #[test]
    fn architecture_in_docs_subdir_counts() {
        let repo = fixture();
        std::fs::write(
            repo.path().join("mods/good/README.md"),
            "# good\n\n## Quickstart\nx\n",
        )
        .unwrap();
        std::fs::create_dir_all(repo.path().join("mods/good/docs")).unwrap();
        std::fs::write(
            repo.path().join("mods/good/docs/ARCHITECTURE.md"),
            "```mermaid\nflowchart TD\na-->b\n```\n```mermaid\nsequenceDiagram\na->>b: x\n```",
        )
        .unwrap();
        let diff = diff_for("mods/good/src/lib.rs", &["fn f() {}"]);
        let report = scan(repo.path(), &diff);
        assert!(
            report.findings.is_empty(),
            "docs/ARCHITECTURE.md must satisfy the presence checks: {:?}",
            report.findings
        );
    }

    #[test]
    fn new_config_surface_without_doc_touch_is_stale() {
        let repo = fixture();
        let diff = diff_for(
            "mods/good/src/main.rs",
            &["    #[arg(long, env = \"GOOD_KNOB\")]", "    knob: String,"],
        );
        let report = scan(repo.path(), &diff);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].rule, "docs-stale-config");
    }

    #[test]
    fn config_surface_with_doc_touch_in_same_module_is_fine() {
        let repo = fixture();
        let mut diff = diff_for(
            "mods/good/src/main.rs",
            &["    #[arg(long, env = \"GOOD_KNOB\")]"],
        );
        diff.push_str(&diff_for(
            "mods/good/docs/CONFIG.md",
            &["| GOOD_KNOB | … |"],
        ));
        let report = scan(repo.path(), &diff);
        assert!(
            report.findings.is_empty(),
            "doc moved with the code — no drift: {:?}",
            report.findings
        );
    }

    #[test]
    fn new_route_without_doc_touch_is_stale_api() {
        let repo = fixture();
        let diff = diff_for(
            "mods/good/src/server.rs",
            &["        .route(\"/v1/things\", get(list_things))"],
        );
        let report = scan(repo.path(), &diff);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].rule, "docs-stale-api");
    }

    #[test]
    fn docs_only_change_enforces_nothing() {
        let repo = fixture();
        // Even in the bare module: touching only docs must not trigger the lane.
        let diff = diff_for("mods/bare/NOTES.md", &["words"]);
        let report = scan(repo.path(), &diff);
        assert!(report.findings.is_empty());
    }

    #[test]
    fn tests_and_vendored_code_are_not_source_surface() {
        let repo = fixture();
        let diff = diff_for("mods/bare/tests/it.rs", &["#[arg(long)] x: u8,"]);
        let report = scan(repo.path(), &diff);
        assert!(
            report.findings.is_empty(),
            "test code must not trigger the lane"
        );
    }
}
