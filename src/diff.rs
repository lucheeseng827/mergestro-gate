// SPDX-License-Identifier: Apache-2.0
//! Diff detection against the base ref — Phase 2, pure-Rust via `gix`.
//!
//! Phase 1 shelled out to `git`. Phase 2 uses gitoxide so the gate has no
//! dependency on a `git` binary at the point of diffing and parses no
//! subprocess text. We compute a two-tree diff `base -> head` and synthesise
//! the unified diff `cargo-mutants --in-diff` consumes.
//!
//! Merge-base resolution is intentionally *not* done here: the GitHub Action
//! resolves the merge-base with `git merge-base` (git is always on the runner)
//! and passes the resolved SHA as `--base`, so this stays a clean two-tree
//! diff. For local use, pass a base ref directly.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gix::bstr::ByteSlice;
use gix::diff::blob::unified_diff::{ConsumeBinaryHunk, ContextSize};
use gix::diff::blob::{Algorithm, Diff, InternedInput, UnifiedDiff};
use gix::object::tree::diff::Change;

/// A changed non-Rust source file plus the new-side line numbers the diff
/// touched, so an engine's mutants can be scoped to the changed lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Repo-relative path.
    pub path: String,
    /// New-side line numbers of added (`+`) lines, ascending.
    pub added_lines: Vec<u32>,
}

/// Back-compat alias: the Python adapter named this `PyFileChange`.
pub type PyFileChange = FileChange;

/// Result of inspecting the diff between base and head.
pub struct DiffScope {
    /// Path to the unified diff file written for `--in-diff`.
    pub diff_path: PathBuf,
    /// Changed `*.rs` files (repo-relative).
    pub changed_rust_files: Vec<String>,
    /// Changed `*.py` files with their touched line numbers (Python adapter).
    pub changed_python_files: Vec<FileChange>,
    /// Changed JS/TS files with their touched line numbers (Stryker adapter).
    pub changed_js_files: Vec<FileChange>,
    /// Changed Go files with their touched line numbers (gremlins adapter).
    pub changed_go_files: Vec<FileChange>,
    /// Changed Java/Kotlin files with their touched line numbers (PIT adapter).
    pub changed_jvm_files: Vec<FileChange>,
}

/// Compute the `base -> head` diff and the changed Rust files, writing the
/// unified diff into `work_dir` for `cargo-mutants --in-diff`.
pub fn compute_scope(
    repo_path: &Path,
    base_ref: &str,
    head_ref: &str,
    work_dir: &Path,
) -> Result<DiffScope> {
    let repo = gix::open(repo_path)
        .with_context(|| format!("opening git repository at {}", repo_path.display()))?;

    let base_tree = peel_to_tree(&repo, base_ref)?;
    let head_tree = peel_to_tree(&repo, head_ref)?;

    let mut unified = String::new();
    let mut changed_rust_files = Vec::new();
    let mut changed_python_files = Vec::new();
    let mut changed_js_files = Vec::new();
    let mut changed_go_files = Vec::new();
    let mut changed_jvm_files = Vec::new();
    let mut first_err: Option<anyhow::Error> = None;

    let mut platform = base_tree.changes().context("starting tree diff")?;
    // Populate `location` on each change so we can attribute hunks to a file.
    platform.options(|opts| {
        opts.track_path();
    });
    platform
        .for_each_to_obtain_tree(&head_tree, |change| {
            // Errors are captured out-of-band so the closure error type stays
            // `Infallible`; we break the walk on the first failure.
            match render_change(
                change,
                &mut unified,
                &mut changed_rust_files,
                &mut changed_python_files,
                &mut changed_js_files,
                &mut changed_go_files,
                &mut changed_jvm_files,
            ) {
                Ok(()) => Ok::<_, std::convert::Infallible>(ControlFlow::Continue(())),
                Err(e) => {
                    first_err = Some(e);
                    Ok(ControlFlow::Break(()))
                }
            }
        })
        .context("diffing base tree against head tree")?;

    if let Some(e) = first_err {
        return Err(e);
    }

    let diff_path = work_dir.join("changed.diff");
    std::fs::write(&diff_path, &unified)
        .with_context(|| format!("writing diff to {}", diff_path.display()))?;

    // Stable, de-duplicated order regardless of tree traversal order.
    changed_rust_files.sort();
    changed_rust_files.dedup();
    changed_python_files.sort_by(|a, b| a.path.cmp(&b.path));
    changed_python_files.dedup();
    changed_js_files.sort_by(|a, b| a.path.cmp(&b.path));
    changed_js_files.dedup();
    changed_go_files.sort_by(|a, b| a.path.cmp(&b.path));
    changed_go_files.dedup();
    changed_jvm_files.sort_by(|a, b| a.path.cmp(&b.path));
    changed_jvm_files.dedup();

    Ok(DiffScope {
        diff_path,
        changed_rust_files,
        changed_python_files,
        changed_js_files,
        changed_go_files,
        changed_jvm_files,
    })
}

/// Resolve a revspec to its tree, peeling through tags/commits as needed.
fn peel_to_tree<'repo>(repo: &'repo gix::Repository, revspec: &str) -> Result<gix::Tree<'repo>> {
    let commit = repo
        .rev_parse_single(revspec)
        .with_context(|| format!("resolving ref `{revspec}` (is it fetched?)"))?
        .object()
        .with_context(|| format!("reading object for `{revspec}`"))?
        .peel_to_kind(gix::object::Kind::Commit)
        .with_context(|| format!("`{revspec}` does not point at a commit"))?
        .into_commit();
    commit
        .tree()
        .with_context(|| format!("reading tree of `{revspec}`"))
}

/// Inspect a single changed file (added/modified/rewritten). Rust files are
/// appended to the unified diff `cargo-mutants --in-diff` consumes; Python files
/// are recorded with their touched line numbers for the Python engine. Other
/// extensions and deletions are skipped — there is nothing on the head side to
/// mutate.
fn render_change(
    change: Change<'_, '_, '_>,
    unified: &mut String,
    changed_rust_files: &mut Vec<String>,
    changed_python_files: &mut Vec<FileChange>,
    changed_js_files: &mut Vec<FileChange>,
    changed_go_files: &mut Vec<FileChange>,
    changed_jvm_files: &mut Vec<FileChange>,
) -> Result<()> {
    let (location, old, new) = match change {
        Change::Addition { location, id, .. } => (location, Vec::new(), blob_data(id)?),
        Change::Modification {
            location,
            previous_id,
            id,
            ..
        } => (location, blob_data(previous_id)?, blob_data(id)?),
        Change::Rewrite {
            location,
            source_id,
            id,
            ..
        } => (location, blob_data(source_id)?, blob_data(id)?),
        Change::Deletion { .. } => return Ok(()),
    };

    let path = location.to_str_lossy().into_owned();

    if path.ends_with(".rs") {
        if let Some(body) = unified_body(&old, &new)? {
            // Prepend the git-style file headers the hunk renderer omits, so
            // cargo-mutants can attribute hunks to a file.
            unified.push_str(&format!(
                "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n"
            ));
            unified.push_str(&body);
            changed_rust_files.push(path);
        }
    } else if path.ends_with(".py") {
        if let Some(body) = unified_body(&old, &new)? {
            let added_lines = added_new_lines(&body);
            if !added_lines.is_empty() {
                changed_python_files.push(FileChange { path, added_lines });
            }
        }
    } else if is_js_source(&path) {
        if let Some(body) = unified_body(&old, &new)? {
            let added_lines = added_new_lines(&body);
            if !added_lines.is_empty() {
                changed_js_files.push(FileChange { path, added_lines });
            }
        }
    } else if is_go_source(&path) {
        if let Some(body) = unified_body(&old, &new)? {
            let added_lines = added_new_lines(&body);
            if !added_lines.is_empty() {
                changed_go_files.push(FileChange { path, added_lines });
            }
        }
    } else if is_jvm_source(&path) {
        if let Some(body) = unified_body(&old, &new)? {
            let added_lines = added_new_lines(&body);
            if !added_lines.is_empty() {
                changed_jvm_files.push(FileChange { path, added_lines });
            }
        }
    }
    Ok(())
}

/// A Go source file gremlins can mutate. Excludes `_test.go` — test files hold
/// the tests, not the code under mutation.
fn is_go_source(path: &str) -> bool {
    path.ends_with(".go") && !path.ends_with("_test.go")
}

/// A Java/Kotlin source file PIT can mutate. `.kts` (Gradle scripts) excluded.
fn is_jvm_source(path: &str) -> bool {
    path.ends_with(".java") || path.ends_with(".kt")
}

/// A JavaScript/TypeScript source Stryker can mutate. Excludes TypeScript
/// declaration files (`.d.ts`) — they carry types, not runnable logic.
fn is_js_source(path: &str) -> bool {
    if path.ends_with(".d.ts") {
        return false;
    }
    [".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// Parse the new-side line numbers of added (`+`) lines from a unified-diff
/// body (hunks without file headers), so Python mutants can be scoped to the
/// changed lines. Tracks the running new-line counter off each `@@` header.
fn added_new_lines(body: &str) -> Vec<u32> {
    let mut lines = Vec::new();
    let mut new_line = 0u32;
    for raw in body.lines() {
        if let Some(rest) = raw.strip_prefix("@@") {
            // `@@ -a,b +c,d @@` — take `c` as the new-side start.
            if let Some(start) = rest
                .split('+')
                .nth(1)
                .and_then(|s| s.trim().split([',', ' ']).next())
                .and_then(|n| n.parse::<u32>().ok())
            {
                new_line = start;
            }
        } else if let Some(content) = raw.strip_prefix('+') {
            if !content.starts_with("++") {
                lines.push(new_line);
                new_line += 1;
            }
        } else if raw.starts_with('-') && !raw.starts_with("--") {
            // Removed line: no new-side advance.
        } else {
            // Context line advances the new-side counter.
            new_line = new_line.saturating_add(1);
        }
    }
    lines
}

/// Build the unified-diff hunks (without file headers) for one blob pair.
/// Returns `None` when there is no textual difference. Rust sources are UTF-8,
/// so lossy conversion is safe for line-level diffing.
fn unified_body(old: &[u8], new: &[u8]) -> Result<Option<String>> {
    let old = String::from_utf8_lossy(old);
    let new = String::from_utf8_lossy(new);
    let input = InternedInput::new(old.as_ref(), new.as_ref());
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let body: String = UnifiedDiff::new(
        &diff,
        &input,
        ConsumeBinaryHunk::new(String::new(), "\n"),
        ContextSize::symmetrical(3),
    )
    .consume()
    .context("rendering unified diff")?;
    if body.is_empty() {
        Ok(None)
    } else {
        Ok(Some(body))
    }
}

/// Read the raw bytes of a blob object.
fn blob_data(id: gix::Id<'_>) -> Result<Vec<u8>> {
    Ok(id
        .object()
        .with_context(|| format!("reading blob {id}"))?
        .data
        .clone())
}

#[cfg(test)]
mod tests {
    /// Parse the `+`-side file paths out of a unified diff, mirroring how
    /// `cargo-mutants` attributes hunks. Used to assert our synthesised diff is
    /// well-formed.
    fn plus_files(diff: &str) -> Vec<String> {
        diff.lines()
            .filter_map(|l| l.strip_prefix("+++ b/"))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn synthesised_headers_are_parseable() {
        let diff = "diff --git a/src/x.rs b/src/x.rs\n--- a/src/x.rs\n+++ b/src/x.rs\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(plus_files(diff), vec!["src/x.rs"]);
    }

    #[test]
    fn added_new_lines_tracks_the_new_side_counter() {
        // One line replaced at line 2; new-side line 2 is the added line.
        let body =
            "@@ -1,3 +1,3 @@\n def is_adult(age):\n-    return age > 17\n+    return age >= 18\n";
        assert_eq!(super::added_new_lines(body), vec![2]);
    }

    #[test]
    fn added_new_lines_handles_multiple_hunks_and_inserts() {
        // Hunk 1 inserts a line at new-side 5; hunk 2 adds lines at 20 and 21.
        let body = "\
@@ -4,2 +4,3 @@
 ctx
+inserted
 ctx
@@ -18,2 +19,3 @@
 ctx
+added_a
+added_b
";
        assert_eq!(super::added_new_lines(body), vec![5, 20, 21]);
    }

    #[test]
    fn is_js_source_recognizes_js_and_ts_extensions() {
        for ext in ["js", "jsx", "mjs", "cjs", "ts", "tsx"] {
            let p = format!("src/x.{ext}");
            assert!(super::is_js_source(&p), "{p} must be a JS source");
        }
    }

    #[test]
    fn is_js_source_excludes_declarations_and_other_files() {
        assert!(!super::is_js_source("src/types.d.ts"));
        assert!(!super::is_js_source("src/lib.rs"));
        assert!(!super::is_js_source("README.md"));
    }

    #[test]
    fn is_go_source_recognizes_go_extension() {
        assert!(super::is_go_source("pkg/calc.go"));
        assert!(super::is_go_source("cmd/main.go"));
    }

    #[test]
    fn is_go_source_excludes_test_files_and_other_extensions() {
        // _test.go files hold tests, not code under mutation — the `!` guard.
        assert!(!super::is_go_source("pkg/calc_test.go"));
        assert!(!super::is_go_source("src/lib.rs"));
        assert!(!super::is_go_source("service.py"));
    }

    #[test]
    fn compute_scope_records_go_changes_with_added_lines() {
        // A modified `.go` file adding a line must be recorded with its touched
        // line numbers. Guards the `!added_lines.is_empty()` gate in the Go
        // branch of render_change: deleting the `!` would drop non-empty files.
        let scope = scope_for_change(
            &[("calc.go", "package main\n\nfunc Add(a, b int) int { return a + b }\n")],
            &[("calc.go", "package main\n\nfunc Add(a, b int) int { return a + b + 0 }\n")],
        );
        let go: Vec<&str> = scope
            .changed_go_files
            .iter()
            .map(|c| c.path.as_str())
            .collect();
        assert_eq!(go, vec!["calc.go"], "the changed .go file must be recorded");
        assert!(!scope.changed_go_files[0].added_lines.is_empty());
    }

    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git available");
        assert!(out.status.success(), "git {args:?} failed");
    }

    /// Build a repo with a base commit then a head commit changing `head_files`,
    /// run `compute_scope` over `HEAD~1..HEAD`, and return the scope.
    fn scope_for_change(
        base_files: &[(&str, &str)],
        head_files: &[(&str, &str)],
    ) -> super::DiffScope {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]);
        git(p, &["config", "user.email", "t@t.io"]);
        git(p, &["config", "user.name", "t"]);
        git(p, &["config", "commit.gpgsign", "false"]);
        for (rel, content) in base_files {
            std::fs::write(p.join(rel), content).unwrap();
        }
        git(p, &["add", "-A"]);
        git(p, &["commit", "-q", "--no-gpg-sign", "-m", "base"]);
        for (rel, content) in head_files {
            std::fs::write(p.join(rel), content).unwrap();
        }
        git(p, &["add", "-A"]);
        git(p, &["commit", "-q", "--no-gpg-sign", "-m", "head"]);
        super::compute_scope(p, "HEAD~1", "HEAD", p).unwrap()
    }

    #[test]
    fn compute_scope_records_py_and_js_changes_with_added_lines() {
        // A modified `.py` and `.js` file, each adding a line, must be recorded
        // with their touched line numbers. Guards the `!added_lines.is_empty()`
        // gates on both the Python and JS branches of render_change: deleting the
        // `!` would drop these (non-empty) files instead of keeping them.
        let scope = scope_for_change(
            &[
                ("calc.py", "def f():\n    return 1\n"),
                ("calc.js", "function f() {\n  return 1;\n}\n"),
            ],
            &[
                ("calc.py", "def f():\n    return 2\n"),
                ("calc.js", "function f() {\n  return 2;\n}\n"),
            ],
        );
        let py: Vec<&str> = scope
            .changed_python_files
            .iter()
            .map(|c| c.path.as_str())
            .collect();
        let js: Vec<&str> = scope
            .changed_js_files
            .iter()
            .map(|c| c.path.as_str())
            .collect();
        assert_eq!(py, vec!["calc.py"], "the changed .py file must be recorded");
        assert_eq!(js, vec!["calc.js"], "the changed .js file must be recorded");
        assert!(!scope.changed_python_files[0].added_lines.is_empty());
        assert!(!scope.changed_js_files[0].added_lines.is_empty());
    }
}
