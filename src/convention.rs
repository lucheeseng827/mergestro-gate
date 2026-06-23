// SPDX-License-Identifier: Apache-2.0
//! Pattern lane — convention / hallucinated-API (Track B, lane 3): the cheap,
//! deterministic slice.
//!
//! Full **convention RAG** — nearest-neighbour of the change against an embedded
//! corpus of the repo's own idioms — is the deferred, *managed/paid* piece: it
//! needs an embedding store and is the GA lane. This module ships the part that
//! is local, free, and needs no model: catching the most common agent failure —
//! a `use` of a crate the project **doesn't depend on** (a hallucinated crate).
//!
//! Method: union the allowed crate roots from every `Cargo.toml` in the repo
//! (dependency keys + package names) with the std roots, then flag any `use`
//! on an *added* line whose root segment isn't among them. Union-of-all-manifests
//! is deliberately permissive — it only ever *misses* a hallucination, never
//! invents one, so false positives stay near zero for an advisory signal. If no
//! manifest is found at all, the lane stays silent (it can't know the deps).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use syn::{Item, UseTree};

use crate::pattern::{PatternFinding, PatternReport};

const W_UNKNOWN_CRATE: u32 = 30;
/// Bound the manifest walk so a pathological tree can't stall the gate.
const MAX_MANIFESTS: usize = 256;

/// Scan changed Rust files for `use`s of undeclared crates, scoped to the diff's
/// added lines. Silent when no `Cargo.toml` is found (deps unknowable).
pub fn scan_files(
    repo: &Path,
    files: &[String],
    added: &BTreeMap<String, BTreeSet<u32>>,
) -> PatternReport {
    let (allowed, found_manifest) = allowed_roots(repo);
    if !found_manifest {
        return PatternReport::default();
    }

    let mut findings = Vec::new();
    let mut seen: BTreeSet<(String, u32, String)> = BTreeSet::new();
    for file in files {
        let Some(added_lines) = added.get(file) else {
            continue;
        };
        if added_lines.is_empty() {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(repo.join(file)) else {
            continue;
        };
        let Ok(ast) = syn::parse_file(&src) else {
            continue;
        };
        for item in &ast.items {
            let Item::Use(u) = item else { continue };
            let mut roots = Vec::new();
            collect_use_roots(&u.tree, &mut roots);
            for (root, line) in roots {
                if !added_lines.contains(&line) || allowed.contains(&root) {
                    continue;
                }
                if !seen.insert((root.clone(), line, file.clone())) {
                    continue;
                }
                findings.push(PatternFinding {
                    rule: "unknown-crate-import".to_string(),
                    file: file.to_string(),
                    line,
                    message: format!(
                        "`use {root}::…` — `{root}` is not a declared dependency (possible hallucinated crate)"
                    ),
                    weight: W_UNKNOWN_CRATE,
                });
            }
        }
    }
    PatternReport::from_findings(findings)
}

/// Root segments of a `use` tree, each with its source line. A top-level group
/// (`use {a, b::c}`) contributes each branch's root; globs contribute nothing.
fn collect_use_roots(tree: &UseTree, out: &mut Vec<(String, u32)>) {
    match tree {
        UseTree::Path(p) => out.push((p.ident.to_string(), p.ident.span().start().line as u32)),
        UseTree::Name(n) => out.push((n.ident.to_string(), n.ident.span().start().line as u32)),
        UseTree::Rename(r) => out.push((r.ident.to_string(), r.ident.span().start().line as u32)),
        UseTree::Glob(_) => {}
        UseTree::Group(g) => {
            for t in &g.items {
                collect_use_roots(t, out);
            }
        }
    }
}

/// Allowed crate roots = std roots + path keywords + every dependency key and
/// package name across all manifests in the repo. The bool is whether any
/// manifest was found at all (false also when the walk was truncated, so the
/// lane stays silent rather than emitting false positives from a partial set).
fn allowed_roots(repo: &Path) -> (BTreeSet<String>, bool) {
    let mut roots: BTreeSet<String> = ["std", "core", "alloc", "crate", "self", "super", "Self"]
        .into_iter()
        .map(str::to_string)
        .collect();
    let (manifests, truncated) = find_manifests(repo);
    if truncated {
        // Allow-list would be incomplete; fail open to avoid false positives.
        return (roots, false);
    }
    let found = !manifests.is_empty();
    for manifest in manifests {
        if let Ok(txt) = std::fs::read_to_string(&manifest) {
            collect_manifest_names(&txt, &mut roots);
        }
    }
    (roots, found)
}

/// Find up to [`MAX_MANIFESTS`] `Cargo.toml` files under `repo`, skipping
/// `target`, `.git`, and `node_modules`. Returns `(paths, truncated)` where
/// `truncated` is `true` when the cap was hit before the walk finished.
fn find_manifests(repo: &Path) -> (Vec<PathBuf>, bool) {
    let mut found = Vec::new();
    let mut stack = vec![repo.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if found.len() >= MAX_MANIFESTS {
            return (found, true);
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let skip = matches!(
                    path.file_name().and_then(|n| n.to_str()),
                    Some("target") | Some(".git") | Some("node_modules")
                );
                if !skip {
                    stack.push(path);
                }
            } else if path.file_name().and_then(|n| n.to_str()) == Some("Cargo.toml") {
                found.push(path);
            }
        }
    }
    (found, false)
}

/// Pull dependency keys and the package name out of one `Cargo.toml`'s text.
/// A line-oriented parser — enough for the table shapes Cargo uses, and the
/// union is forgiving, so we don't need a full TOML parse.
fn collect_manifest_names(txt: &str, roots: &mut BTreeSet<String>) {
    let mut in_deps = false;
    let mut in_package = false;
    for raw in txt.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some(sec) = line.strip_prefix('[') {
            let sec = sec.trim_end_matches(']').trim();
            in_package = sec == "package";
            in_deps = false;
            // Dotted table `[dependencies.foo]` / `[target.'cfg(..)'.dependencies.bar]`
            // names the dependency directly; its body lines are that dep's fields.
            if let Some(idx) = sec.rfind("dependencies.") {
                let dep = sec[idx + "dependencies.".len()..]
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'');
                if !dep.is_empty() {
                    roots.insert(normalize(dep));
                }
            } else if sec == "dependencies" || sec.ends_with("dependencies") {
                in_deps = true;
            }
            continue;
        }
        if in_package {
            if let Some(rest) = line.strip_prefix("name") {
                if let Some(v) = rest.split('=').nth(1) {
                    let v = v.trim().trim_matches('"');
                    if !v.is_empty() {
                        roots.insert(normalize(v));
                    }
                }
            }
        } else if in_deps {
            // `key = ...`, `key.workspace = true`, `key = { ... }` — take the
            // identifier before the first `=`, `.`, or space.
            let key = line
                .split(|c: char| c == '=' || c == '.' || c.is_whitespace())
                .next()
                .unwrap_or("")
                .trim();
            if !key.is_empty() {
                roots.insert(normalize(key));
            }
        }
    }
}

/// Crate import names use underscores; Cargo keys may use hyphens.
fn normalize(name: &str) -> String {
    name.replace('-', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(txt: &str) -> BTreeSet<String> {
        let mut s = BTreeSet::new();
        collect_manifest_names(txt, &mut s);
        s
    }

    #[test]
    fn manifest_parse_gets_deps_and_package() {
        let txt = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
serde = "1"
tokio-util = { version = "0.7" }
gix.workspace = true

[dev-dependencies]
tempfile = "3"

[dependencies.fancy]
version = "2"
"#;
        let n = names(txt);
        assert!(n.contains("my_crate")); // package name, normalized
        assert!(n.contains("serde"));
        assert!(n.contains("tokio_util")); // hyphen → underscore
        assert!(n.contains("gix"));
        assert!(n.contains("tempfile"));
        assert!(n.contains("fancy")); // dotted dependency table
        // A dependency's own field lines must not leak in as crate names.
        assert!(!n.contains("version"));
    }

    #[test]
    fn manifest_parse_handles_quoted_dotted_dep() {
        // Dotted table with a quoted key: [dependencies."hyp-henated"].
        // Guards the `trim_matches(|c| c == '"' || c == '\'')` call: when
        // `||` is replaced with `&&`, no char satisfies both conditions so
        // quotes are never stripped, and the name leaks in with surrounding
        // quotes instead of being normalised.
        let txt = "[dependencies.\"hyp-henated\"]\nversion = \"1\"\n";
        let n = names(txt);
        assert!(n.contains("hyp_henated"), "quoted dotted dep must be stripped and normalised");
    }

    /// Scan a single source string against an explicit allowed set, treating
    /// every line as added. Returns the flagged roots.
    fn flag(src: &str, allowed: &[&str]) -> Vec<String> {
        let allowed: BTreeSet<String> = allowed.iter().map(|s| s.to_string()).collect();
        let mut findings = Vec::new();
        let mut seen = BTreeSet::new();
        let ast = syn::parse_file(src).unwrap();
        for item in &ast.items {
            let Item::Use(u) = item else { continue };
            let mut roots = Vec::new();
            collect_use_roots(&u.tree, &mut roots);
            for (root, line) in roots {
                if allowed.contains(&root) || !seen.insert((root.clone(), line)) {
                    continue;
                }
                findings.push(root);
            }
        }
        findings
    }

    #[test]
    fn flags_undeclared_crate() {
        let f = flag("use made_up_crate::Thing;\n", &["std", "serde"]);
        assert_eq!(f, vec!["made_up_crate"]);
    }

    #[test]
    fn allows_declared_crate_and_std_and_crate_keyword() {
        let src = "use serde::Serialize;\nuse std::collections::HashMap;\nuse crate::foo::bar;\n";
        let f = flag(src, &["std", "core", "alloc", "crate", "serde"]);
        assert!(f.is_empty());
    }

    #[test]
    fn top_level_group_checks_each_root() {
        let f = flag("use {serde::Serialize, ghost::X};\n", &["serde"]);
        assert_eq!(f, vec!["ghost"]);
    }

    #[test]
    fn full_scan_is_silent_without_a_manifest() {
        // No Cargo.toml under the dir → lane stays silent.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "use ghost::X;\n").unwrap();
        let added: BTreeMap<String, BTreeSet<u32>> =
            [("src/lib.rs".to_string(), [1u32].into_iter().collect())].into();
        let r = scan_files(dir.path(), &["src/lib.rs".to_string()], &added);
        assert!(r.findings.is_empty());
    }

    #[test]
    fn full_scan_flags_against_real_manifest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n[dependencies]\nserde = \"1\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "use serde::Serialize;\nuse ghost_crate::Thing;\n",
        )
        .unwrap();
        // Only line 2 is "added".
        let added: BTreeMap<String, BTreeSet<u32>> =
            [("src/lib.rs".to_string(), [2u32].into_iter().collect())].into();
        let r = scan_files(dir.path(), &["src/lib.rs".to_string()], &added);
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].rule, "unknown-crate-import");
        assert_eq!(r.findings[0].line, 2);
        assert!(r.findings[0].message.contains("ghost_crate"));
    }

    #[test]
    fn find_manifests_traverses_subdirectories() {
        // Cargo.toml is ONLY in a subdirectory — the DFS must descend into it.
        // Guards the `if !skip { stack.push(path) }` branch: when `!` is deleted,
        // subdirectories are never pushed, so the manifest is never found and the
        // lane silently returns no findings even when an import is hallucinated.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub/src")).unwrap();
        std::fs::write(
            dir.path().join("sub/Cargo.toml"),
            "[package]\nname = \"sub\"\n[dependencies]\nchrono = \"0.4\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("sub/src/lib.rs"),
            "use chrono::Utc;\nuse ghost_crate::Thing;\n",
        )
        .unwrap();
        let added: BTreeMap<String, BTreeSet<u32>> = [(
            "sub/src/lib.rs".to_string(),
            [1u32, 2u32].into_iter().collect(),
        )]
        .into();
        let r = scan_files(dir.path(), &["sub/src/lib.rs".to_string()], &added);
        // chrono is declared; ghost_crate is not — exactly one finding.
        assert_eq!(r.findings.len(), 1, "subdirectory Cargo.toml must be discovered");
        assert!(r.findings[0].message.contains("ghost_crate"));
    }

    #[test]
    fn same_unknown_crate_on_same_line_in_two_files_both_reported() {
        // Guards the file-inclusive dedup key `(root, line, file)`.
        // With the old `(root, line)` key, the second file's finding was
        // dropped because (root="ghost", line=1) was already in `seen`.
        let dir = tempfile::tempdir().unwrap();
        let manifest = "[package]\nname = \"demo\"\n[dependencies]\n";
        std::fs::write(dir.path().join("Cargo.toml"), manifest).unwrap();
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::create_dir_all(dir.path().join("b")).unwrap();
        std::fs::write(dir.path().join("a/lib.rs"), "use ghost::X;\n").unwrap();
        std::fs::write(dir.path().join("b/lib.rs"), "use ghost::X;\n").unwrap();
        let added: BTreeMap<String, BTreeSet<u32>> = [
            ("a/lib.rs".to_string(), [1u32].into_iter().collect()),
            ("b/lib.rs".to_string(), [1u32].into_iter().collect()),
        ]
        .into();
        let r = scan_files(
            dir.path(),
            &["a/lib.rs".to_string(), "b/lib.rs".to_string()],
            &added,
        );
        assert_eq!(r.findings.len(), 2, "each file must produce its own finding");
    }

    #[test]
    fn truncated_manifest_walk_stays_silent() {
        // Create MAX_MANIFESTS+1 dirs each containing a Cargo.toml so the cap
        // is actually hit, then verify find_manifests flags truncation and
        // allowed_roots fails open (returns found=false → scan_files silent).
        let dir = tempfile::tempdir().unwrap();
        for i in 0..=MAX_MANIFESTS {
            let sub = dir.path().join(format!("m{i}"));
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("Cargo.toml"), "[package]\nname=\"demo\"\n").unwrap();
        }
        let (_paths, truncated) = find_manifests(dir.path());
        assert!(truncated, "find_manifests must report truncation when the cap is hit");
        let (_roots, found) = allowed_roots(dir.path());
        assert!(!found, "truncated allow-list must fail open (found=false)");
    }

    #[test]
    fn declared_import_on_added_line_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n[dependencies]\nserde = \"1\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "use serde::Serialize;\n").unwrap();
        let added: BTreeMap<String, BTreeSet<u32>> =
            [("src/lib.rs".to_string(), [1u32].into_iter().collect())].into();
        assert!(scan_files(dir.path(), &["src/lib.rs".to_string()], &added)
            .findings
            .is_empty());
    }
}
