// SPDX-License-Identifier: Apache-2.0
//! The mutation phase: enumerate mutants on the changed surface, cap them
//! per function, run `cargo-mutants`, and read back the outcomes.
//!
//! Where the cost lives (per the build plan): mutant *count* and baseline
//! build time. `--in-diff` scopes mutation to changed lines; the per-function
//! cap bounds the count further. We enforce the cap by listing candidates,
//! selecting the surplus, and excluding it from the run via `--exclude-re`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::Config;
use crate::report::Mutant;
use crate::runner::CommandRunner;

/// Outcomes read back from a `cargo-mutants` run.
#[derive(Debug, Default, Clone)]
pub struct MutationResults {
    pub survivors: Vec<Mutant>,
    pub caught: usize,
    pub timed_out: usize,
    pub unviable: usize,
}

impl MutationResults {
    /// Mutants that actually completed a test run (excludes unviable, which
    /// never compiled).
    pub fn tested(&self) -> usize {
        self.caught + self.survivors.len() + self.timed_out
    }
}

/// Result of applying the per-function cap.
pub struct CapSelection {
    /// Mutants kept for testing.
    pub kept: Vec<Mutant>,
    /// Mutants dropped by the cap; excluded from the run.
    pub excluded: Vec<Mutant>,
}

/// Enumerate candidate mutants on the diff (`cargo mutants --in-diff --list`).
///
/// `packages` scopes mutation to the same crate(s) used in [`run_mutation`] so
/// that list and run enumerate an identical set of mutants — without matching
/// scopes, cargo-mutants can produce a different count in each phase, which the
/// accounting check treats as an operational failure.
pub fn list_candidates(
    runner: &dyn CommandRunner,
    repo: &Path,
    diff_path: &Path,
    packages: &[String],
) -> Result<Vec<Mutant>> {
    let diff = diff_path.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec!["mutants", "--in-diff", &diff, "--list", "--json"];
    // Mirror the --package scoping used by run_mutation so both phases see
    // the same mutation surface and their counts stay in sync.
    let pkg_strs: Vec<String> = packages
        .iter()
        .flat_map(|p| ["--package".to_string(), p.clone()])
        .collect();
    let pkg_refs: Vec<&str> = pkg_strs.iter().map(String::as_str).collect();
    args.extend_from_slice(&pkg_refs);
    let out = runner
        .run("cargo", &args, repo)
        .context("running `cargo mutants --list --json`")?;
    if !out.success {
        bail!("cargo mutants --list failed:\n{}", out.combined());
    }
    parse_list_json(&out.stdout).context("parsing `cargo mutants --list --json` output")
}

/// Apply the per-function cap. Within each function (grouped by
/// [`Mutant::group_key`]) the first `cap` mutants are kept in listing order;
/// the rest are excluded. Listing order is deterministic, so the cap is too.
pub fn apply_cap(candidates: Vec<Mutant>, cap: usize) -> CapSelection {
    use std::collections::HashMap;
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut kept = Vec::new();
    let mut excluded = Vec::new();
    for m in candidates {
        let count = seen.entry(m.group_key()).or_insert(0);
        if *count < cap {
            *count += 1;
            kept.push(m);
        } else {
            excluded.push(m);
        }
    }
    CapSelection { kept, excluded }
}

/// Build the `cargo mutants` argument list for a diff-scoped run.
fn mutation_args(
    cfg: &Config,
    diff_path: &Path,
    output_dir: &Path,
    excluded: &[Mutant],
    packages: &[String],
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "mutants".into(),
        "--in-diff".into(),
        diff_path.to_string_lossy().into_owned(),
        "--jobs".into(),
        cfg.jobs.to_string(),
        "--timeout".into(),
        cfg.timeout_secs.to_string(),
        "--output".into(),
        output_dir.to_string_lossy().into_owned(),
    ];
    // Faster test runner when requested: nextest runs each test in its own
    // process, highly parallel. cargo-mutants accepts `--test-tool nextest`.
    if cfg.test_tool == "nextest" {
        args.push("--test-tool".into());
        args.push("nextest".into());
    }
    // Scope *mutation* to the package(s) the changed files belong to — we only
    // want to mutate the changed crate, and this avoids building unrelated crates
    // as mutants. With no resolved package we omit `--package` and fall back to
    // cargo-mutants' default (whole workspace), which is always safe.
    //
    // Crucially, by default we still run the *whole workspace's* tests against
    // each mutant (`--test-workspace`), so a mutant caught only by a downstream
    // crate's tests is still caught — scoping mutation must not silently turn
    // those into false survivors. Teams that want the faster (narrower) behaviour
    // opt in via `test_changed_package_only`.
    if !packages.is_empty() {
        for p in packages {
            args.push("--package".into());
            args.push(p.clone());
        }
        if !cfg.test_changed_package_only {
            args.push("--test-workspace".into());
            args.push("true".into());
        }
    }
    // When the determinism pre-flight is skipped, the caller has asserted the
    // unmutated tree is already green (e.g. CI ran the suite). cargo-mutants'
    // own baseline build+test then just re-pays that full cycle, so skip it —
    // a meaningful saving since the baseline is one of the costliest cycles.
    if cfg.skip_preflight {
        args.push("--baseline".into());
        args.push("skip".into());
    }
    for m in excluded {
        args.push("--exclude-re".into());
        args.push(exclude_pattern(m));
    }
    args
}

/// Resolve the Cargo package name(s) the changed files belong to, by walking up
/// from each file to its nearest `Cargo.toml` with a `[package] name`. Used to
/// scope cargo-mutants in a workspace. De-duplicated; empty when none resolve.
pub fn changed_packages(repo: &Path, changed_rust_files: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for rel in changed_rust_files {
        if let Some(name) = nearest_package(repo, rel) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

/// Walk up from `rel` (a repo-relative file) to find the nearest `Cargo.toml`
/// with a `[package] name`, without escaping `repo`.
fn nearest_package(repo: &Path, rel: &str) -> Option<String> {
    let mut dir = repo.join(rel);
    dir.pop(); // drop the file name
    loop {
        if let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) {
            if let Some(name) = package_name(&text) {
                return Some(name);
            }
        }
        if dir == repo || !dir.pop() {
            return None;
        }
    }
}

/// Extract `name` from a `Cargo.toml`'s `[package]` table (line-oriented).
fn package_name(toml: &str) -> Option<String> {
    let mut in_package = false;
    for raw in toml.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_package = line.trim_end_matches(']').trim_start_matches('[').trim() == "package";
            continue;
        }
        if in_package {
            // Match the exact `name` key — not any key starting with "name"
            // (e.g. `name.workspace` or `name_x`), which would feed a bogus
            // value to `--package`.
            if let Some((key, val)) = line.split_once('=') {
                if key.trim() == "name" {
                    let v = val.trim().trim_matches('"');
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Run `cargo-mutants` over the diff, excluding the capped-out surplus, and
/// read the outcome files back. `packages` scopes the build/test to the changed
/// crate(s) (empty = whole workspace).
///
/// `expected` is the number of kept (non-excluded) candidates. Every kept
/// mutant must land in exactly one outcome bucket — caught, survived, timed
/// out, or unviable. If the engine accounts for a number *different* from
/// `expected`, the run is untrustworthy: *fewer* means it died part-way (e.g.
/// cargo-mutants' Windows temp-path bug) after creating its output directory;
/// *more* means inconsistent/partial output. Either way the survivor count
/// can't be trusted, so treat it as an operational failure, not a silent pass.
pub fn run_mutation(
    runner: &dyn CommandRunner,
    cfg: &Config,
    diff_path: &Path,
    output_dir: &Path,
    excluded: &[Mutant],
    packages: &[String],
    expected: usize,
) -> Result<MutationResults> {
    let args = mutation_args(cfg, diff_path, output_dir, excluded, packages);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = runner
        .run("cargo", &arg_refs, &cfg.repo)
        .context("running `cargo mutants`")?;

    // cargo-mutants exits non-zero *when survivors are found* — that's a
    // signal, not an error. Distinguish "found survivors" from "couldn't run"
    // by whether it produced its output directory.
    let mutants_out = output_dir.join("mutants.out");
    if !mutants_out.is_dir() {
        bail!(
            "cargo mutants did not produce results (exit {:?}):\n{}",
            out.code,
            out.combined()
        );
    }
    let results = read_results(&mutants_out)?;

    // Validate the outcome count against the expected kept count.
    //
    // * Over-count (with viable mutations): engine produced MORE tested outcomes
    //   than kept candidates — the survivor set could be understated; bail.
    // * Over-count (all unviable): no mutation compiled, so no survivor can
    //   escape. The +N gap is a systematic list/run enumeration difference in
    //   the cargo-mutants tool (not corrupted output). Warn and continue —
    //   the gate decision (survivors == 0) is correct regardless.
    // * Zero-count: engine produced NOTHING — it almost certainly died
    //   immediately after creating the output dir; bail.
    // * Non-zero under-count: `--list --json` can include mutants (e.g. in
    //   `#[test]` functions) that `cargo mutants` silently skips at runtime.
    //   The survivor set is still valid, so emit a warning and continue.
    // * Exact match: fully accounted; continue.
    let accounted = results.tested() + results.unviable;
    if accounted > expected {
        if results.tested() == 0 {
            eprintln!(
                "slop-gate: warning: cargo mutants accounted for {accounted} mutant(s), \
                 expected {expected}; all {accounted} were unviable (exit {:?}). \
                 Proceeding — survivor count is zero regardless.",
                out.code
            );
        } else {
            bail!(
                "cargo mutants accounted for {accounted} mutant(s), expected {expected}; \
                 inconsistent output (exit {:?}):\n{}",
                out.code,
                out.combined()
            );
        }
    }
    if accounted == 0 && expected > 0 {
        bail!(
            "cargo mutants accounted for 0 mutant(s), expected {expected}; \
             the run likely failed immediately after creating its output directory \
             (exit {:?}):\n{}",
            out.code,
            out.combined()
        );
    }
    if accounted < expected {
        eprintln!(
            "slop-gate: warning: cargo mutants tested {accounted} mutant(s) but \
             {expected} were listed; some mutants were silently skipped \
             (exit {:?}). Proceeding with available results.",
            out.code
        );
    }
    Ok(results)
}

/// Read the `mutants.out/*.txt` outcome files into [`MutationResults`].
fn read_results(mutants_out: &Path) -> Result<MutationResults> {
    let survivors = read_mutant_file(&mutants_out.join("missed.txt"));
    let caught = read_mutant_file(&mutants_out.join("caught.txt")).len();
    let timed_out = read_mutant_file(&mutants_out.join("timeout.txt")).len();
    let unviable = read_mutant_file(&mutants_out.join("unviable.txt")).len();
    Ok(MutationResults {
        survivors,
        caught,
        timed_out,
        unviable,
    })
}

/// Parse one outcome file: one mutant name per line. Missing file → empty.
fn read_mutant_file(path: &Path) -> Vec<Mutant> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines().filter_map(Mutant::parse_name).collect()
}

/// Parse `cargo mutants --list --json` output into mutants, tolerant of schema
/// drift across `cargo-mutants` versions.
fn parse_list_json(stdout: &str) -> Result<Vec<Mutant>> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).context("not valid JSON")?;
    let items = value
        .as_array()
        .context("expected a JSON array of mutants")?;

    let mut mutants = Vec::with_capacity(items.len());
    for item in items {
        let Some(mut m) = mutant_from_json(item) else {
            continue;
        };
        // Prefer the structured function name when the list provides one.
        if let Some(func) = json_function(item) {
            m.function = Some(func);
        }
        mutants.push(m);
    }
    Ok(mutants)
}

/// Build a [`Mutant`] from one JSON list element, preferring the `name` field
/// and falling back to discrete `file`/`line`/`column` fields.
fn mutant_from_json(item: &serde_json::Value) -> Option<Mutant> {
    if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
        if let Some(m) = Mutant::parse_name(name) {
            return Some(m);
        }
    }
    let file = item.get("file")?.as_str()?.to_string();
    let line = item.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let column = item.get("column").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let description = item
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some(Mutant {
        name: format!("{file}:{line}:{column}: {description}"),
        file,
        line,
        column,
        function: None,
        description,
    })
}

/// Extract the enclosing function name from a list element, handling both the
/// string and `{ "function_name": ... }` object shapes.
fn json_function(item: &serde_json::Value) -> Option<String> {
    match item.get("function") {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Object(o)) => o
            .get("function_name")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        _ => None,
    }
}

/// Build an anchored `--exclude-re` pattern matching exactly one mutant.
///
/// The pattern has to be the mutant's *whole* name, not its `file:line:column`
/// prefix. `cargo-mutants` emits a whole set of mutants at a single location —
/// one per candidate replacement value for the enclosing function's return type
/// — and every one of them shares that prefix, so a prefix pattern excludes the
/// capped-out mutant along with the siblings the cap meant to keep. The cap then
/// silently tests fewer mutants than it accounted for; and where *every* kept
/// mutant has a capped-out sibling at its own line and column, it removes the
/// selection entirely. `cargo-mutants` then reports "No mutants to filter",
/// exits 0 and writes no output directory, which [`run_mutation`] cannot
/// distinguish from a crashed run — so the gate fails on a PR with nothing
/// wrong with it. Anchoring both ends of the full name leaves exactly the kept
/// set behind.
fn exclude_pattern(m: &Mutant) -> String {
    format!("^{}$", regex_escape(&m.name))
}

/// Escape regex metacharacters so a literal file path matches as text.
fn regex_escape(s: &str) -> String {
    const META: &[char] = &[
        '.', '^', '$', '*', '+', '?', '(', ')', '[', ']', '{', '}', '|', '\\',
    ];
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if META.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Where this run's `cargo-mutants` output should be written.
pub fn output_dir_for(work_dir: &Path) -> PathBuf {
    work_dir.join("mutants-run")
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    fn mutant(file: &str, line: u32, func: Option<&str>) -> Mutant {
        Mutant {
            file: file.into(),
            line,
            column: 1,
            function: func.map(str::to_string),
            description: "replace x".into(),
            name: format!("{file}:{line}:1: replace x"),
        }
    }

    #[test]
    fn cap_keeps_first_n_per_function() {
        let candidates = vec![
            mutant("a.rs", 1, Some("foo")),
            mutant("a.rs", 2, Some("foo")),
            mutant("a.rs", 3, Some("foo")),
            mutant("a.rs", 9, Some("bar")),
        ];
        let sel = apply_cap(candidates, 2);
        // foo capped at 2, bar untouched → 3 kept, 1 excluded.
        assert_eq!(sel.kept.len(), 3);
        assert_eq!(sel.excluded.len(), 1);
        assert_eq!(sel.excluded[0].line, 3);
    }

    #[test]
    fn cap_groups_by_file_when_function_unknown() {
        let candidates = vec![
            mutant("a.rs", 1, None),
            mutant("a.rs", 2, None),
            mutant("b.rs", 1, None),
        ];
        let sel = apply_cap(candidates, 1);
        // a.rs capped to 1, b.rs to 1 → 2 kept, 1 excluded.
        assert_eq!(sel.kept.len(), 2);
        assert_eq!(sel.excluded.len(), 1);
    }

    #[test]
    fn mutation_args_skip_baseline_only_when_preflight_skipped() {
        use crate::config::Config;
        let diff = Path::new("changed.diff");
        let out = Path::new("out");

        // skip_preflight = true → pass `--baseline skip`.
        let cfg = Config {
            skip_preflight: true,
            ..Config::default()
        };
        let args = mutation_args(&cfg, diff, out, &[], &[]);
        let joined = args.join(" ");
        assert!(joined.contains("--baseline skip"), "got: {joined}");
        assert!(joined.contains("--in-diff"));

        // skip_preflight = false → no baseline override (cargo-mutants default).
        let cfg = Config {
            skip_preflight: false,
            ..Config::default()
        };
        assert!(!mutation_args(&cfg, diff, out, &[], &[])
            .iter()
            .any(|a| a == "--baseline"));
    }

    #[test]
    fn mutation_args_passes_nextest_when_configured() {
        use crate::config::Config;
        let cargo = Config::default(); // test_tool = "cargo"
        assert!(
            !mutation_args(&cargo, Path::new("d"), Path::new("o"), &[], &[])
                .iter()
                .any(|a| a == "--test-tool")
        );
        let nextest = Config {
            test_tool: "nextest".into(),
            ..Config::default()
        };
        let joined = mutation_args(&nextest, Path::new("d"), Path::new("o"), &[], &[]).join(" ");
        assert!(joined.contains("--test-tool nextest"), "got: {joined}");
    }

    #[test]
    fn mutation_args_scopes_mutation_to_packages_but_tests_workspace() {
        use crate::config::Config;
        let cfg = Config::default(); // test_changed_package_only = false
        let joined = mutation_args(
            &cfg,
            Path::new("d.diff"),
            Path::new("out"),
            &[],
            &["mergestro-gate".to_string(), "other".to_string()],
        )
        .join(" ");
        assert!(joined.contains("--package mergestro-gate"));
        assert!(joined.contains("--package other"));
        // Default: mutate the changed crate(s) but run the whole workspace's
        // tests, so downstream catches aren't lost (no false survivors).
        assert!(joined.contains("--test-workspace true"), "got: {joined}");

        // No packages → no --package and no --test-workspace (cargo-mutants
        // default whole-workspace mutation already tests the workspace).
        let none = mutation_args(&cfg, Path::new("d.diff"), Path::new("out"), &[], &[]);
        assert!(!none.iter().any(|a| a == "--package"));
        assert!(!none.iter().any(|a| a == "--test-workspace"));
    }

    #[test]
    fn test_changed_package_only_narrows_tests() {
        use crate::config::Config;
        let cfg = Config {
            test_changed_package_only: true,
            ..Config::default()
        };
        let joined = mutation_args(
            &cfg,
            Path::new("d.diff"),
            Path::new("out"),
            &[],
            &["mergestro-gate".to_string()],
        )
        .join(" ");
        // Opt-in: still mutate only the changed crate, but DON'T force workspace
        // tests — faster, at the cost of possible downstream-only false survivors.
        assert!(joined.contains("--package mergestro-gate"));
        assert!(!joined.contains("--test-workspace"));
    }

    #[test]
    fn package_name_parses_package_table_only() {
        let toml = "[package]\nname = \"mergestro-gate\"\nversion = \"0.4.0\"\n[dependencies]\nname_thing = \"1\"\n";
        assert_eq!(package_name(toml).as_deref(), Some("mergestro-gate"));
        // No [package] name → None.
        assert_eq!(package_name("[dependencies]\nserde = \"1\"\n"), None);
        // Keys that merely *start* with "name" must not match (would feed a
        // bogus value to --package): `name.workspace = true`, `name_x = "..."`.
        assert_eq!(
            package_name("[package]\nname.workspace = true\nversion = \"0.1\"\n"),
            None
        );
        assert_eq!(package_name("[package]\nname_x = \"oops\"\n"), None);
    }

    #[test]
    fn changed_packages_resolves_nearest_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("crates/foo/src")).unwrap();
        std::fs::write(
            root.join("crates/foo/Cargo.toml"),
            "[package]\nname = \"foo\"\n",
        )
        .unwrap();
        std::fs::write(root.join("crates/foo/src/lib.rs"), "// x\n").unwrap();
        let pkgs = changed_packages(root, &["crates/foo/src/lib.rs".to_string()]);
        assert_eq!(pkgs, vec!["foo".to_string()]);
        // A file under no manifest resolves to nothing.
        assert!(changed_packages(root, &["README.md".to_string()]).is_empty());
    }

    #[test]
    fn regex_escape_escapes_path_metachars() {
        assert_eq!(regex_escape("src/foo.rs"), "src/foo\\.rs");
        assert_eq!(regex_escape("a+b(c).rs"), "a\\+b\\(c\\)\\.rs");
    }

    #[test]
    fn exclude_pattern_is_anchored() {
        let m = mutant("src/foo.rs", 12, Some("f"));
        assert_eq!(exclude_pattern(&m), "^src/foo\\.rs:12:1: replace x$");
    }

    /// `cargo-mutants` puts many mutants at one `file:line:column` — one per candidate
    /// replacement value — so an exclusion keyed on the location alone takes the siblings with
    /// it. The pattern must name the one mutant it is for and leave the rest alone.
    #[test]
    fn exclude_pattern_spares_siblings_at_the_same_location() {
        let capped =
            Mutant::parse_name("src/foo.rs:12:5: replace f -> Option<u32> with None").unwrap();
        let kept =
            Mutant::parse_name("src/foo.rs:12:5: replace f -> Option<u32> with Some(0)").unwrap();

        let re = Regex::new(&exclude_pattern(&capped)).expect("valid regex");
        assert!(
            re.is_match(&capped.name),
            "must still exclude its own mutant"
        );
        assert!(
            !re.is_match(&kept.name),
            "must not exclude a sibling at the same location"
        );
    }

    /// The end the whole cap exists for: whatever `apply_cap` keeps must survive the exclusions
    /// built from what it dropped. When it does not, the run tests fewer mutants than the gate
    /// accounted for — and once every kept mutant is matched, `cargo-mutants` has nothing left to
    /// run, writes no output directory, and the gate reports an operational failure against a PR
    /// that is fine.
    #[test]
    fn the_cap_never_excludes_a_mutant_it_kept() {
        // One function, sixteen mutants, all at the same line and column: the shape
        // `cargo-mutants` produces for a function whose return type has many candidate
        // replacement values. The replacements carry regex metacharacters on purpose.
        let candidates: Vec<Mutant> = (0..16)
            .map(|i| {
                let mut m = Mutant::parse_name(&format!(
                    "src/noise.rs:222:9: replace gen -> Result<Vec<f32>> with Ok(vec![{i}.0])"
                ))
                .expect("parses");
                m.function = Some("gen".into());
                m
            })
            .collect();

        let sel = apply_cap(candidates, 5);
        assert_eq!(sel.kept.len(), 5);
        assert_eq!(sel.excluded.len(), 11);

        let patterns: Vec<Regex> = sel
            .excluded
            .iter()
            .map(|m| Regex::new(&exclude_pattern(m)).expect("exclude pattern must be valid regex"))
            .collect();

        for (re, dropped) in patterns.iter().zip(&sel.excluded) {
            assert!(
                re.is_match(&dropped.name),
                "`{}` must exclude itself",
                dropped.name
            );
            for kept in &sel.kept {
                assert!(
                    !re.is_match(&kept.name),
                    "the pattern excluding `{}` also matches the kept `{}`",
                    dropped.name,
                    kept.name
                );
            }
        }
    }

    #[test]
    fn parse_list_json_uses_name_then_function() {
        let json = r#"[
            {"name":"src/a.rs:10:5: replace > with >=","function":"do_it"},
            {"name":"src/b.rs:3:1: replace - with +","function":{"function_name":"calc"}}
        ]"#;
        let mutants = parse_list_json(json).unwrap();
        assert_eq!(mutants.len(), 2);
        assert_eq!(mutants[0].function.as_deref(), Some("do_it"));
        assert_eq!(mutants[0].line, 10);
        assert_eq!(mutants[1].function.as_deref(), Some("calc"));
    }

    #[test]
    fn parse_list_json_falls_back_to_discrete_fields() {
        let json = r#"[{"file":"src/c.rs","line":7,"column":2,"description":"replace y"}]"#;
        let mutants = parse_list_json(json).unwrap();
        assert_eq!(mutants.len(), 1);
        assert_eq!(mutants[0].file, "src/c.rs");
        assert_eq!(mutants[0].line, 7);
        assert_eq!(mutants[0].column, 2);
    }

    #[test]
    fn parse_list_json_handles_empty() {
        assert!(parse_list_json("").unwrap().is_empty());
        assert!(parse_list_json("[]").unwrap().is_empty());
    }

    #[test]
    fn read_mutant_file_missing_is_empty() {
        let p = Path::new("/nonexistent/does/not/exist/missed.txt");
        assert!(read_mutant_file(p).is_empty());
    }

    #[test]
    fn zero_accounted_bails_when_expected_nonzero() {
        use crate::runner::test_support::ScriptedRunner;
        use tempfile::tempdir;

        // Simulate cargo-mutants creating its output dir but producing no outcomes
        // at all — the most reliable signal that the run died immediately.
        let work = tempdir().unwrap();
        let output_dir = work.path().join("out");
        let mutants_out = output_dir.join("mutants.out");
        std::fs::create_dir_all(&mutants_out).unwrap();
        // No caught.txt / missed.txt written → 0 outcomes.

        let runner = ScriptedRunner::new();
        runner.push_ok(""); // the `cargo mutants` run "succeeds"

        let cfg = Config {
            repo: work.path().to_path_buf(),
            ..Config::default()
        };
        let err = run_mutation(
            &runner,
            &cfg,
            &work.path().join("diff.patch"),
            &output_dir,
            &[],
            &[],
            3, // expected 3, but 0 accounted
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("accounted for 0 mutant(s), expected 3"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn non_zero_under_accounted_warns_but_does_not_bail() {
        use crate::runner::test_support::ScriptedRunner;
        use tempfile::tempdir;

        // Simulate cargo-mutants silently skipping some mutants (e.g. test-code
        // mutations listed by --list but filtered at runtime). 3 expected, 1
        // accounted — non-zero, so proceed with available results.
        let work = tempdir().unwrap();
        let output_dir = work.path().join("out");
        let mutants_out = output_dir.join("mutants.out");
        std::fs::create_dir_all(&mutants_out).unwrap();
        std::fs::write(mutants_out.join("caught.txt"), "src/a.rs:1:1: replace x\n").unwrap();

        let runner = ScriptedRunner::new();
        runner.push_ok("");

        let cfg = Config {
            repo: work.path().to_path_buf(),
            ..Config::default()
        };
        let results = run_mutation(
            &runner,
            &cfg,
            &work.path().join("diff.patch"),
            &output_dir,
            &[],
            &[],
            3, // expected 3, only 1 accounted — non-zero under-count → Ok
        )
        .expect("non-zero under-count must not bail");
        assert_eq!(results.caught, 1);
    }

    #[test]
    fn all_unviable_run_accounts_correctly() {
        use crate::runner::test_support::ScriptedRunner;
        use tempfile::tempdir;

        // All 3 mutants were unviable (failed to compile). tested() = 0,
        // unviable = 3, expected = 3 → exact match → Ok.
        // Kills the `+ → -` mutation on `results.tested() + results.unviable`:
        // with `-`, `0 - 3` underflows (panic in debug), failing this test.
        let work = tempdir().unwrap();
        let output_dir = work.path().join("out");
        let mutants_out = output_dir.join("mutants.out");
        std::fs::create_dir_all(&mutants_out).unwrap();
        std::fs::write(
            mutants_out.join("unviable.txt"),
            "src/a.rs:1:1: replace x\nsrc/a.rs:2:1: replace y\nsrc/a.rs:3:1: replace z\n",
        )
        .unwrap();

        let runner = ScriptedRunner::new();
        runner.push_ok("");

        let cfg = Config {
            repo: work.path().to_path_buf(),
            ..Config::default()
        };
        let results = run_mutation(
            &runner,
            &cfg,
            &work.path().join("diff.patch"),
            &output_dir,
            &[],
            &[],
            3, // 3 expected, 3 unviable → exact match
        )
        .expect("all-unviable run must not bail when count matches");
        assert_eq!(results.unviable, 3);
        assert_eq!(results.tested(), 0);
    }

    #[test]
    fn all_unviable_over_count_warns_but_does_not_bail() {
        use crate::runner::test_support::ScriptedRunner;
        use tempfile::tempdir;

        // cargo-mutants found 1 more unviable mutant in the run than the list
        // predicted. When every outcome is unviable (tested() == 0), the
        // survivor count is provably zero — the over-count gap is a systematic
        // list/run enumeration difference in cargo-mutants, not corrupted
        // output. The gate must warn and continue, not bail.
        let work = tempdir().unwrap();
        let output_dir = work.path().join("out");
        let mutants_out = output_dir.join("mutants.out");
        std::fs::create_dir_all(&mutants_out).unwrap();
        std::fs::write(
            mutants_out.join("unviable.txt"),
            "src/a.rs:1:1: replace x\nsrc/a.rs:2:1: replace y\nsrc/a.rs:3:1: replace z\nsrc/a.rs:4:1: replace w\n",
        )
        .unwrap();

        let runner = ScriptedRunner::new();
        runner.push_ok("");

        let cfg = Config {
            repo: work.path().to_path_buf(),
            ..Config::default()
        };
        let results = run_mutation(
            &runner,
            &cfg,
            &work.path().join("diff.patch"),
            &output_dir,
            &[],
            &[],
            3, // 3 expected, 4 unviable → over-count but all unviable → warn, not bail
        )
        .expect("all-unviable over-count must not bail");
        assert_eq!(results.unviable, 4);
        assert_eq!(results.tested(), 0);
        assert!(results.survivors.is_empty());
    }

    #[test]
    fn over_accounted_run_bails_instead_of_passing() {
        use crate::runner::test_support::ScriptedRunner;
        use tempfile::tempdir;

        // The engine recorded *more* outcomes than were kept (inconsistent /
        // partial output): 1 kept, but 2 accounted (1 caught + 1 survived).
        let work = tempdir().unwrap();
        let output_dir = work.path().join("out");
        let mutants_out = output_dir.join("mutants.out");
        std::fs::create_dir_all(&mutants_out).unwrap();
        std::fs::write(mutants_out.join("caught.txt"), "src/a.rs:1:1: replace x\n").unwrap();
        std::fs::write(mutants_out.join("missed.txt"), "src/a.rs:2:1: replace y\n").unwrap();

        let runner = ScriptedRunner::new();
        runner.push_ok("");

        let cfg = Config {
            repo: work.path().to_path_buf(),
            ..Config::default()
        };
        let err = run_mutation(
            &runner,
            &cfg,
            &work.path().join("diff.patch"),
            &output_dir,
            &[],
            &[],
            1, // expected 1, but 2 accounted
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("accounted for 2 mutant(s), expected 1"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn fully_accounted_run_succeeds() {
        use crate::runner::test_support::ScriptedRunner;
        use tempfile::tempdir;

        // 2 kept: 1 caught + 1 survived → fully accounted, no bail.
        let work = tempdir().unwrap();
        let output_dir = work.path().join("out");
        let mutants_out = output_dir.join("mutants.out");
        std::fs::create_dir_all(&mutants_out).unwrap();
        std::fs::write(mutants_out.join("caught.txt"), "src/a.rs:1:1: replace x\n").unwrap();
        std::fs::write(mutants_out.join("missed.txt"), "src/a.rs:2:1: replace y\n").unwrap();

        let runner = ScriptedRunner::new();
        runner.push_ok("");

        let cfg = Config {
            repo: work.path().to_path_buf(),
            ..Config::default()
        };
        let results = run_mutation(
            &runner,
            &cfg,
            &work.path().join("diff.patch"),
            &output_dir,
            &[],
            &[],
            2,
        )
        .unwrap();
        assert_eq!(results.caught, 1);
        assert_eq!(results.survivors.len(), 1);
        assert_eq!(results.tested(), 2);
    }
}
