// SPDX-License-Identifier: Apache-2.0
//! Language engines behind one trait.
//!
//! The orchestrator ([`crate::pipeline`]) drives mutation through
//! [`MutationEngine`] so the verdict, severity ranking, debt-delta and
//! telemetry are written once and shared across languages. Adding a language is
//! implementing this trait and registering it in [`default_engines`]; nothing
//! downstream changes.
//!
//! Each engine owns the language-specific bits — which files it claims, its
//! determinism pre-flight, whether its tool is installed (and whether a missing
//! tool is fatal), and how it enumerates + runs mutants. Everything an engine
//! contributes to the report is summarised in [`EngineRun`].

use std::path::Path;

use anyhow::{bail, Result};

use crate::config::Config;
use crate::diff::DiffScope;
use crate::mutants::{self, MutationResults};
use crate::report::PreflightOutcome;
use crate::runner::CommandRunner;
use crate::{golang, js, jvm, preflight, python};

/// One engine's contribution to a [`crate::report::GateReport`].
#[derive(Debug, Default)]
pub struct EngineRun {
    /// Per-mutant outcomes: survivors, caught, timed-out, unviable.
    pub results: MutationResults,
    /// Mutants enumerated on the changed surface (before the cap).
    pub candidates: usize,
    /// Mutants dropped by the per-function cap.
    pub capped_out: usize,
}

/// A per-language mutation engine. The pipeline calls these in registration
/// order; the preflight-sharing and accumulation logic lives in the pipeline so
/// behaviour stays identical across engines.
pub trait MutationEngine {
    /// Human label for logs/telemetry (e.g. `"Rust"`).
    fn language(&self) -> &'static str;

    /// Does the diff touch files this engine handles?
    fn applies(&self, scope: &DiffScope) -> bool;

    /// Determinism pre-flight for this engine's test suite.
    fn preflight(&self, runner: &dyn CommandRunner, cfg: &Config) -> Result<PreflightOutcome>;

    /// Is the engine's tool installed?
    fn available(&self, runner: &dyn CommandRunner, cfg: &Config) -> bool;

    /// Called when [`available`](Self::available) is `false`. The default makes
    /// a missing engine fatal (the gate can't certify what it can't run);
    /// optional/advisory engines override to warn and skip.
    fn on_unavailable(&self, _scope: &DiffScope) -> Result<()> {
        bail!("{} mutation engine not found on PATH", self.language())
    }

    /// Enumerate, cap, and run mutants on the changed surface.
    fn analyze(
        &self,
        runner: &dyn CommandRunner,
        cfg: &Config,
        scope: &DiffScope,
        work_dir: &Path,
    ) -> Result<EngineRun>;
}

/// The registered engines, in run order. Rust first so it owns the shared
/// pre-flight slot when a change touches both languages.
pub fn default_engines() -> Vec<Box<dyn MutationEngine>> {
    vec![
        Box::new(RustEngine),
        Box::new(PythonEngine),
        Box::new(JsEngine),
        Box::new(GoEngine),
        Box::new(JvmEngine),
    ]
}

/// Rust via `cargo-mutants`: the reference adapter (per-function cap, accounted
/// run). A missing engine is fatal.
pub struct RustEngine;

impl MutationEngine for RustEngine {
    /// Returns `"Rust"`.
    fn language(&self) -> &'static str {
        "Rust"
    }

    /// Returns `true` when the diff contains at least one changed Rust file.
    fn applies(&self, scope: &DiffScope) -> bool {
        !scope.changed_rust_files.is_empty()
    }

    /// Delegates to [`preflight::run`], which runs `cargo test` the configured
    /// number of times and classifies the outcome.
    fn preflight(&self, runner: &dyn CommandRunner, cfg: &Config) -> Result<PreflightOutcome> {
        preflight::run(runner, cfg)
    }

    /// Probes `cargo mutants --version`. cargo-mutants is a cargo subcommand,
    /// so the binary is invoked through cargo rather than directly (direct
    /// invocation is rejected by recent versions).
    fn available(&self, runner: &dyn CommandRunner, cfg: &Config) -> bool {
        runner
            .run("cargo", &["mutants", "--version"], &cfg.repo)
            .map(|o| o.success)
            .unwrap_or(false)
    }

    /// Bails with an install hint — cargo-mutants is required for Rust mutation
    /// and the gate cannot certify correctness without it.
    fn on_unavailable(&self, _scope: &DiffScope) -> Result<()> {
        bail!(
            "cargo-mutants not found on PATH. Install it with:\n  \
             cargo install cargo-mutants"
        )
    }

    /// Enumerates candidates via `cargo mutants --list --json`, applies the
    /// per-function cap, runs the surviving selection, and returns the counts
    /// alongside the raw mutation results.
    fn analyze(
        &self,
        runner: &dyn CommandRunner,
        cfg: &Config,
        scope: &DiffScope,
        work_dir: &Path,
    ) -> Result<EngineRun> {
        let candidates = mutants::list_candidates(runner, &cfg.repo, &scope.diff_path)?;
        let enumerated = candidates.len();
        let selection = mutants::apply_cap(candidates, cfg.max_mutants_per_function);
        let kept = selection.kept.len();
        // No mutable mutants on the changed surface (e.g. the diff only touched
        // comments, `use` lines, formatting, or test-only code). `cargo mutants`
        // would print "No mutants to filter", exit 0, and write no output dir —
        // which `run_mutation` can't distinguish from a crashed run. Nothing to
        // verify here, so short-circuit to an empty (clean) result instead.
        if kept == 0 {
            return Ok(EngineRun {
                results: MutationResults::default(),
                candidates: enumerated,
                capped_out: selection.excluded.len(),
            });
        }
        let output_dir = mutants::output_dir_for(work_dir);
        // Scope the run to the changed crate(s) so a workspace doesn't rebuild
        // and re-test every package per mutant.
        let packages = mutants::changed_packages(&cfg.repo, &scope.changed_rust_files);
        let results = mutants::run_mutation(
            runner,
            cfg,
            &scope.diff_path,
            &output_dir,
            &selection.excluded,
            &packages,
            kept,
        )?;
        Ok(EngineRun {
            results,
            candidates: enumerated,
            capped_out: selection.excluded.len(),
        })
    }
}

/// Python via `cosmic-ray` (advisory PoC): enumerate+run are combined and there
/// is no per-function cap yet, so `candidates == tested`. A missing engine is a
/// warning, not a hard failure, so the gate stays usable where it isn't set up.
pub struct PythonEngine;

impl MutationEngine for PythonEngine {
    /// Returns `"Python"`.
    fn language(&self) -> &'static str {
        "Python"
    }

    /// Returns `true` when the diff contains at least one changed Python file.
    fn applies(&self, scope: &DiffScope) -> bool {
        !scope.changed_python_files.is_empty()
    }

    /// Runs the configured `python_test_command` via [`preflight::run_command`].
    fn preflight(&self, runner: &dyn CommandRunner, cfg: &Config) -> Result<PreflightOutcome> {
        preflight::run_command(
            runner,
            &cfg.python_test_command,
            &cfg.repo,
            cfg.preflight_runs,
            cfg.skip_preflight,
        )
    }

    /// Delegates to [`python::is_available`], which probes `cosmic-ray --version`.
    fn available(&self, runner: &dyn CommandRunner, cfg: &Config) -> bool {
        python::is_available(runner, &cfg.repo)
    }

    /// Emits a non-fatal warning and returns `Ok(())` so the gate continues
    /// without Python mutation. Bails if called with no changed Python files,
    /// which would indicate a programming error in the caller.
    fn on_unavailable(&self, scope: &DiffScope) -> Result<()> {
        let n = scope.changed_python_files.len();
        if n == 0 {
            bail!(
                "PythonEngine::on_unavailable called with no changed Python files; \
                   this is a programming error (applies() must be true before this is called)"
            );
        }
        eprintln!(
            "slop-gate: warning: cosmic-ray not found; skipping {n} changed Python file(s). \
             Install it with `pip install cosmic-ray`."
        );
        Ok(())
    }

    /// Runs cosmic-ray over all changed Python files via [`python::run`]. There
    /// is no per-function cap yet, so `candidates` equals the tested count and
    /// `capped_out` is always `0`.
    fn analyze(
        &self,
        runner: &dyn CommandRunner,
        cfg: &Config,
        scope: &DiffScope,
        work_dir: &Path,
    ) -> Result<EngineRun> {
        let results = python::run(runner, cfg, &scope.changed_python_files, work_dir)?;
        let candidates = results.tested();
        Ok(EngineRun {
            results,
            candidates,
            capped_out: 0,
        })
    }
}

/// TS/JS via `Stryker` (advisory PoC): like Python, enumerate+run are combined
/// (`candidates == tested`, no per-function cap) and a missing or unconfigured
/// engine warns and skips rather than failing the gate.
pub struct JsEngine;

impl MutationEngine for JsEngine {
    /// Returns `"JS/TS"`.
    fn language(&self) -> &'static str {
        "JS/TS"
    }

    /// Returns `true` when the diff contains at least one changed JS/TS file.
    fn applies(&self, scope: &DiffScope) -> bool {
        !scope.changed_js_files.is_empty()
    }

    /// Stryker runs its own initial test run (it aborts on a red baseline), so
    /// the gate's pre-flight is skipped for this engine.
    fn preflight(&self, _runner: &dyn CommandRunner, _cfg: &Config) -> Result<PreflightOutcome> {
        Ok(PreflightOutcome::Skipped)
    }

    /// Usable only when Stryker is installed *and* a test-runner plugin is
    /// declared in `package.json` — otherwise the run can't be configured.
    fn available(&self, runner: &dyn CommandRunner, cfg: &Config) -> bool {
        js::is_available(runner, &cfg.repo) && js::detect_runner(&cfg.repo).is_some()
    }

    /// Non-fatal: warns and continues without JS mutation.
    fn on_unavailable(&self, scope: &DiffScope) -> Result<()> {
        let n = scope.changed_js_files.len();
        if n == 0 {
            bail!(
                "JsEngine::on_unavailable called with no changed JS/TS files; \
                   this is a programming error (applies() must be true before this is called)"
            );
        }
        eprintln!(
            "slop-gate: warning: Stryker not found or no @stryker-mutator/*-runner declared; \
             skipping {n} changed JS/TS file(s)."
        );
        Ok(())
    }

    /// Runs Stryker over all changed JS/TS files via [`js::run`]. No per-function
    /// cap yet, so `candidates` equals the tested count and `capped_out` is `0`.
    fn analyze(
        &self,
        runner: &dyn CommandRunner,
        cfg: &Config,
        scope: &DiffScope,
        work_dir: &Path,
    ) -> Result<EngineRun> {
        let results = js::run(runner, cfg, &scope.changed_js_files, work_dir)?;
        let candidates = results.tested();
        Ok(EngineRun {
            results,
            candidates,
            capped_out: 0,
        })
    }
}

/// Go via `gremlins` (advisory PoC): like Python/JS, enumerate+run are combined
/// (`candidates == tested`, no per-function cap) and a missing engine warns and
/// skips rather than failing the gate. gremlins runs `go test`, which self-
/// checks the baseline, so the gate pre-flight is `Skipped`.
pub struct GoEngine;

impl MutationEngine for GoEngine {
    /// Returns `"Go"`.
    fn language(&self) -> &'static str {
        "Go"
    }

    /// Returns `true` when the diff contains at least one changed Go file.
    fn applies(&self, scope: &DiffScope) -> bool {
        !scope.changed_go_files.is_empty()
    }

    /// gremlins runs `go test` itself (aborting on a red baseline), so the
    /// gate's pre-flight is skipped for this engine.
    fn preflight(&self, _runner: &dyn CommandRunner, _cfg: &Config) -> Result<PreflightOutcome> {
        Ok(PreflightOutcome::Skipped)
    }

    /// Delegates to [`golang::is_available`], which probes `gremlins --version`.
    fn available(&self, runner: &dyn CommandRunner, cfg: &Config) -> bool {
        golang::is_available(runner, &cfg.repo)
    }

    /// Non-fatal: warns and continues without Go mutation.
    fn on_unavailable(&self, scope: &DiffScope) -> Result<()> {
        let n = scope.changed_go_files.len();
        if n == 0 {
            bail!(
                "GoEngine::on_unavailable called with no changed Go files; \
                   this is a programming error (applies() must be true before this is called)"
            );
        }
        eprintln!(
            "slop-gate: warning: gremlins not found; skipping {n} changed Go file(s). \
             Install it from https://gremlins.dev."
        );
        Ok(())
    }

    /// Runs gremlins over the module via [`golang::run`]. No per-function cap
    /// yet, so `candidates` equals the tested count and `capped_out` is `0`.
    fn analyze(
        &self,
        runner: &dyn CommandRunner,
        cfg: &Config,
        scope: &DiffScope,
        work_dir: &Path,
    ) -> Result<EngineRun> {
        let results = golang::run(runner, cfg, &scope.changed_go_files, work_dir)?;
        let candidates = results.tested();
        Ok(EngineRun {
            results,
            candidates,
            capped_out: 0,
        })
    }
}

/// Java/Kotlin via `PIT` (advisory PoC): like the other non-Rust engines,
/// enumerate+run are combined (`candidates == tested`, no per-function cap) and
/// a missing build tool warns and skips. PIT runs the suite itself, so the
/// gate pre-flight is `Skipped`.
pub struct JvmEngine;

impl MutationEngine for JvmEngine {
    /// Returns `"Java/Kotlin"`.
    fn language(&self) -> &'static str {
        "Java/Kotlin"
    }

    /// Returns `true` when the diff contains at least one changed `.java`/`.kt`.
    fn applies(&self, scope: &DiffScope) -> bool {
        !scope.changed_jvm_files.is_empty()
    }

    /// PIT runs the suite via Maven/Gradle, which self-checks the baseline, so
    /// the gate pre-flight is skipped for this engine.
    fn preflight(&self, _runner: &dyn CommandRunner, _cfg: &Config) -> Result<PreflightOutcome> {
        Ok(PreflightOutcome::Skipped)
    }

    /// Usable when a build tool (Maven/Gradle) is present and runnable.
    fn available(&self, runner: &dyn CommandRunner, cfg: &Config) -> bool {
        jvm::is_available(runner, &cfg.repo)
    }

    /// Non-fatal: warns and continues without JVM mutation.
    fn on_unavailable(&self, scope: &DiffScope) -> Result<()> {
        let n = scope.changed_jvm_files.len();
        if n == 0 {
            bail!(
                "JvmEngine::on_unavailable called with no changed Java/Kotlin files; \
                   this is a programming error (applies() must be true before this is called)"
            );
        }
        eprintln!(
            "slop-gate: warning: no runnable Maven/Gradle + PIT setup found; \
             skipping {n} changed Java/Kotlin file(s)."
        );
        Ok(())
    }

    /// Runs PIT via [`jvm::run`]. No per-function cap yet, so `candidates`
    /// equals the tested count and `capped_out` is `0`.
    fn analyze(
        &self,
        runner: &dyn CommandRunner,
        cfg: &Config,
        scope: &DiffScope,
        work_dir: &Path,
    ) -> Result<EngineRun> {
        let results = jvm::run(runner, cfg, &scope.changed_jvm_files, work_dir)?;
        let candidates = results.tested();
        Ok(EngineRun {
            results,
            candidates,
            capped_out: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::diff::{DiffScope, FileChange, PyFileChange};
    use crate::runner::test_support::ScriptedRunner;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn scope(rust: &[&str], py: &[&str]) -> DiffScope {
        DiffScope {
            diff_path: PathBuf::from("diff.patch"),
            changed_rust_files: rust.iter().map(|s| s.to_string()).collect(),
            changed_python_files: py
                .iter()
                .map(|p| PyFileChange {
                    path: p.to_string(),
                    added_lines: vec![],
                })
                .collect(),
            changed_js_files: vec![],
            changed_go_files: vec![],
            changed_jvm_files: vec![],
        }
    }

    /// A scope whose only changed files are JS/TS (with one touched line each).
    fn js_scope(js: &[&str]) -> DiffScope {
        DiffScope {
            diff_path: PathBuf::from("diff.patch"),
            changed_rust_files: vec![],
            changed_python_files: vec![],
            changed_js_files: js
                .iter()
                .map(|p| FileChange {
                    path: p.to_string(),
                    added_lines: vec![1],
                })
                .collect(),
            changed_go_files: vec![],
            changed_jvm_files: vec![],
        }
    }

    /// A scope whose only changed files are Go (with one touched line each).
    fn go_scope(go: &[&str]) -> DiffScope {
        DiffScope {
            diff_path: PathBuf::from("diff.patch"),
            changed_rust_files: vec![],
            changed_python_files: vec![],
            changed_js_files: vec![],
            changed_go_files: go
                .iter()
                .map(|p| FileChange {
                    path: p.to_string(),
                    added_lines: vec![1],
                })
                .collect(),
            changed_jvm_files: vec![],
        }
    }

    /// Write a `package.json` declaring (or not) a Stryker test-runner plugin.
    fn write_package_json(dir: &Path, with_runner: bool) {
        let body = if with_runner {
            r#"{ "devDependencies": { "@stryker-mutator/mocha-runner": "8" } }"#
        } else {
            r#"{ "devDependencies": {} }"#
        };
        std::fs::write(dir.join("package.json"), body).unwrap();
    }

    fn cfg_for(dir: &Path) -> Config {
        Config {
            repo: dir.to_path_buf(),
            ..Config::default()
        }
    }

    #[test]
    fn engines_claim_only_their_language() {
        let rust = RustEngine;
        let py = PythonEngine;
        assert!(rust.applies(&scope(&["a.rs"], &[])));
        assert!(!rust.applies(&scope(&[], &["a.py"])));
        assert!(py.applies(&scope(&[], &["a.py"])));
        assert!(!py.applies(&scope(&["a.rs"], &[])));
    }

    #[test]
    fn default_set_is_rust_python_js_go_then_jvm() {
        let engines = default_engines();
        assert_eq!(engines.len(), 5);
        assert_eq!(engines[0].language(), "Rust");
        assert_eq!(engines[1].language(), "Python");
        assert_eq!(engines[2].language(), "JS/TS");
        assert_eq!(engines[3].language(), "Go");
        assert_eq!(engines[4].language(), "Java/Kotlin");
    }

    #[test]
    fn rust_missing_engine_is_fatal_python_is_not() {
        assert!(RustEngine.on_unavailable(&scope(&["a.rs"], &[])).is_err());
        assert!(PythonEngine.on_unavailable(&scope(&[], &["a.py"])).is_ok());
    }

    #[test]
    fn python_on_unavailable_requires_changed_python_files() {
        // on_unavailable is only called when applies() is true (non-empty Python
        // files). Calling it with an empty set is a programming error; the function
        // should bail so the invariant violation is loud rather than silent.
        assert!(PythonEngine.on_unavailable(&scope(&[], &[])).is_err());
    }

    #[test]
    fn rust_engine_available_when_probe_succeeds() {
        let work = tempdir().unwrap();
        let runner = ScriptedRunner::new();
        runner.push_ok("cargo-mutants 24.0.0");
        assert!(RustEngine.available(&runner, &cfg_for(work.path())));
    }

    #[test]
    fn rust_engine_unavailable_when_probe_fails() {
        let work = tempdir().unwrap();
        let runner = ScriptedRunner::new();
        runner.push_fail(1, "error: no such subcommand: `mutants`");
        assert!(!RustEngine.available(&runner, &cfg_for(work.path())));
    }

    #[test]
    fn python_engine_available_when_cosmic_ray_present() {
        let work = tempdir().unwrap();
        let runner = ScriptedRunner::new();
        runner.push_ok("cosmic-ray 8.0.0");
        assert!(PythonEngine.available(&runner, &cfg_for(work.path())));
    }

    #[test]
    fn python_engine_unavailable_when_cosmic_ray_absent() {
        let work = tempdir().unwrap();
        let runner = ScriptedRunner::new();
        runner.push_fail(127, "command not found: cosmic-ray");
        assert!(!PythonEngine.available(&runner, &cfg_for(work.path())));
    }

    /// Verify the default `on_unavailable` is fatal. A minimal engine that
    /// intentionally does NOT override the method, so the trait's default runs.
    #[test]
    fn default_on_unavailable_is_fatal() {
        struct MinimalEngine;
        impl MutationEngine for MinimalEngine {
            fn language(&self) -> &'static str {
                "Test"
            }
            fn applies(&self, _: &DiffScope) -> bool {
                false
            }
            fn preflight(&self, _: &dyn CommandRunner, _: &Config) -> Result<PreflightOutcome> {
                unimplemented!()
            }
            fn available(&self, _: &dyn CommandRunner, _: &Config) -> bool {
                false
            }
            fn analyze(
                &self,
                _: &dyn CommandRunner,
                _: &Config,
                _: &DiffScope,
                _: &Path,
            ) -> Result<EngineRun> {
                unimplemented!()
            }
        }
        assert!(MinimalEngine.on_unavailable(&scope(&[], &[])).is_err());
    }

    #[test]
    fn rust_engine_analyze_enumerates_and_returns_candidates() {
        let work = tempdir().unwrap();
        let cfg = cfg_for(work.path());
        let sc = scope(&["src/lib.rs"], &[]);

        // Pre-create the output directory so run_mutation can read results.
        let output_dir = crate::mutants::output_dir_for(work.path());
        let mutants_out = output_dir.join("mutants.out");
        std::fs::create_dir_all(&mutants_out).unwrap();
        std::fs::write(
            mutants_out.join("caught.txt"),
            "src/lib.rs:1:1: replace f\n",
        )
        .unwrap();

        let runner = ScriptedRunner::new();
        // list_candidates: cargo mutants --in-diff <diff> --list --json
        runner.push_ok(
            r#"[{"name":"src/lib.rs:1:1: replace f","file":"src/lib.rs","line":1,"column":1}]"#,
        );
        // run_mutation: cargo mutants --in-diff ...
        runner.push_ok("test result: ok");

        let run = RustEngine.analyze(&runner, &cfg, &sc, work.path()).unwrap();
        assert_eq!(
            run.candidates, 1,
            "analyze must report the enumerated candidate count"
        );
        assert_eq!(run.capped_out, 0);
        assert_eq!(run.results.caught, 1);
    }

    #[test]
    fn rust_engine_analyze_short_circuits_on_zero_mutants() {
        // The diff changed a Rust file but `--list` finds no mutable mutants
        // (e.g. only comments / `use` lines changed). cargo-mutants would then
        // exit 0 with "No mutants to filter" and write no output dir. analyze
        // must short-circuit to a clean empty result, NOT call run_mutation
        // (which would bail on the missing output dir). The runner is scripted
        // with ONLY the --list response: if run_mutation were reached it would
        // need a second scripted command and panic, failing this test.
        let work = tempdir().unwrap();
        let cfg = cfg_for(work.path());
        let sc = scope(&["src/lib.rs"], &[]);

        let runner = ScriptedRunner::new();
        runner.push_ok("[]"); // list_candidates: empty — nothing mutable on the diff

        let run = RustEngine.analyze(&runner, &cfg, &sc, work.path()).unwrap();
        assert_eq!(run.candidates, 0);
        assert_eq!(run.capped_out, 0);
        assert_eq!(run.results.tested(), 0);
        assert!(run.results.survivors.is_empty());
    }

    #[test]
    fn python_engine_analyze_returns_tested_count_as_candidates() {
        use crate::diff::PyFileChange;

        let work = tempdir().unwrap();
        let cfg = cfg_for(work.path());
        let sc = DiffScope {
            diff_path: PathBuf::from("diff.patch"),
            changed_rust_files: vec![],
            changed_python_files: vec![PyFileChange {
                path: "adult.py".to_string(),
                added_lines: vec![2],
            }],
            changed_js_files: vec![],
            changed_go_files: vec![],
            changed_jvm_files: vec![],
        };

        let runner = ScriptedRunner::new();
        runner.push_ok("initialized"); // cosmic-ray init
        runner.push_ok("executed"); // cosmic-ray exec
                                    // cosmic-ray dump: one killed mutant on line 2 (within the changed set)
        runner.push_ok(concat!(
            r#"[{"job_id":"j1","mutations":[{"module_path":"adult.py","operator_name":"core/Op","occurrence":0,"#,
            r#""start_pos":[2,1],"end_pos":[2,5],"operator_args":{},"definition_name":"f"}]},"#,
            r#"{"worker_outcome":"normal","output":"","test_outcome":"killed","diff":""}]"#
        ));

        let run = PythonEngine
            .analyze(&runner, &cfg, &sc, work.path())
            .unwrap();
        assert_eq!(
            run.candidates, 1,
            "analyze must set candidates to the number of tested mutants"
        );
        assert_eq!(run.results.caught, 1);
        assert_eq!(run.capped_out, 0);
    }

    #[test]
    fn js_engine_applies_only_to_js() {
        let js = JsEngine;
        assert!(js.applies(&js_scope(&["src/a.ts"])));
        assert!(!js.applies(&scope(&["a.rs"], &["a.py"])));
    }

    #[test]
    fn js_engine_available_requires_both_stryker_and_runner() {
        let work = tempdir().unwrap();
        // Both present → available.
        write_package_json(work.path(), true);
        let runner = ScriptedRunner::new();
        runner.push_ok("8.2.0"); // stryker --version
        assert!(JsEngine.available(&runner, &cfg_for(work.path())));
    }

    #[test]
    fn js_engine_unavailable_when_stryker_missing() {
        // Runner plugin declared, but Stryker itself isn't installed. Guards the
        // `&&` (an `||` would call this available) and the `available -> true`
        // mutation.
        let work = tempdir().unwrap();
        write_package_json(work.path(), true);
        let runner = ScriptedRunner::new();
        runner.push_fail(127, "command not found: stryker");
        assert!(!JsEngine.available(&runner, &cfg_for(work.path())));
    }

    #[test]
    fn js_engine_unavailable_when_runner_plugin_missing() {
        // Stryker installed, but no @stryker-mutator/*-runner in package.json.
        // The other half of the `&&`, and the `available -> true` mutation.
        let work = tempdir().unwrap();
        write_package_json(work.path(), false);
        let runner = ScriptedRunner::new();
        runner.push_ok("8.2.0");
        assert!(!JsEngine.available(&runner, &cfg_for(work.path())));
    }

    #[test]
    fn js_on_unavailable_warns_and_is_ok_with_changed_files() {
        // Non-fatal: with changed JS files present it warns and returns Ok.
        // Guards the `on_unavailable -> Ok(())` mutation (which would skip the
        // bail branch) by pairing with the empty-scope test below.
        assert!(JsEngine.on_unavailable(&js_scope(&["src/a.js"])).is_ok());
    }

    #[test]
    fn js_on_unavailable_bails_without_changed_files() {
        // Invariant: applies() must be true before on_unavailable is called, so
        // an empty JS set is a programming error and must bail. Kills the
        // `== -> !=` mutation on the `n == 0` guard.
        assert!(JsEngine.on_unavailable(&js_scope(&[])).is_err());
    }

    #[test]
    fn go_engine_applies_only_to_go() {
        let go = GoEngine;
        assert!(go.applies(&go_scope(&["pkg/calc.go"])));
        assert!(!go.applies(&scope(&["a.rs"], &["a.py"])));
        assert!(!go.applies(&js_scope(&["src/a.ts"])));
    }

    #[test]
    fn go_engine_available_when_gremlins_present() {
        let work = tempdir().unwrap();
        let runner = ScriptedRunner::new();
        runner.push_ok("gremlins version 0.5.0");
        assert!(GoEngine.available(&runner, &cfg_for(work.path())));
    }

    #[test]
    fn go_engine_unavailable_when_gremlins_missing() {
        // Guards both `available -> true` and `available -> false` mutations.
        let work = tempdir().unwrap();
        let runner = ScriptedRunner::new();
        runner.push_fail(127, "command not found: gremlins");
        assert!(!GoEngine.available(&runner, &cfg_for(work.path())));
    }

    #[test]
    fn go_on_unavailable_warns_and_is_ok_with_changed_files() {
        // Non-fatal: with changed Go files present it warns and returns Ok.
        // Guards the `on_unavailable -> Ok(())` mutation by pairing with the
        // empty-scope test below.
        assert!(GoEngine.on_unavailable(&go_scope(&["pkg/calc.go"])).is_ok());
    }

    #[test]
    fn go_on_unavailable_bails_without_changed_files() {
        // Calling on_unavailable with no Go files is a programming error.
        // Kills the `== -> !=` mutation on the `n == 0` guard.
        assert!(GoEngine.on_unavailable(&go_scope(&[])).is_err());
    }

    #[test]
    fn go_engine_analyze_runs_gremlins_and_returns_results() {
        // Drive analyze end-to-end: script gremlins and pre-write the JSON
        // report so golang::run can parse it. Guards `analyze -> Ok(Default::default())`.
        let work = tempdir().unwrap();
        std::fs::write(
            work.path().join("gremlins-report.json"),
            r#"{
              "files": [
                {
                  "filename": "pkg/calc.go",
                  "mutations": [
                    { "type": "CONDITIONALS_BOUNDARY", "status": "LIVED", "line": 1, "column": 5 },
                    { "type": "ARITHMETIC_BASE", "status": "KILLED", "line": 1, "column": 10 }
                  ]
                }
              ]
            }"#,
        )
        .unwrap();

        let cfg = cfg_for(work.path());
        let sc = go_scope(&["pkg/calc.go"]); // changed line {1}
        let runner = ScriptedRunner::new();
        runner.push_ok("done"); // gremlins unleash ./...

        let run = GoEngine.analyze(&runner, &cfg, &sc, work.path()).unwrap();
        assert_eq!(run.results.survivors.len(), 1);
        assert_eq!(run.results.caught, 1);
        assert_eq!(run.candidates, run.results.tested());
        assert_eq!(run.capped_out, 0);
    }

    #[test]
    fn js_engine_analyze_runs_stryker_and_returns_results() {
        // Drive analyze end-to-end: package.json declares the runner, Stryker is
        // scripted, and the JSON report is pre-written for `js::run` to parse.
        // Guards the `analyze -> Ok(Default::default())` mutation.
        let work = tempdir().unwrap();
        write_package_json(work.path(), true);
        std::fs::write(
            work.path().join("stryker-report.json"),
            r#"{
              "schemaVersion": "1.0",
              "files": {
                "src/calc.js": {
                  "language": "javascript",
                  "source": "…",
                  "mutants": [
                    { "id": "0", "mutatorName": "ArithmeticOperator", "status": "Survived",
                      "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } },
                    { "id": "1", "mutatorName": "EqualityOperator", "status": "Killed",
                      "location": { "start": { "line": 1, "column": 3 }, "end": { "line": 1, "column": 4 } } }
                  ]
                }
              }
            }"#,
        )
        .unwrap();

        let cfg = cfg_for(work.path());
        let sc = js_scope(&["src/calc.js"]); // changed line {1}
        let runner = ScriptedRunner::new();
        runner.push_ok("done"); // stryker run

        let run = JsEngine.analyze(&runner, &cfg, &sc, work.path()).unwrap();
        assert_eq!(run.results.survivors.len(), 1);
        assert_eq!(run.results.caught, 1);
        assert_eq!(run.candidates, run.results.tested());
        assert_eq!(run.capped_out, 0);
    }
}
