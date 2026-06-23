// SPDX-License-Identifier: Apache-2.0
//! `slop-gate` — CLI for the Slop Filter behavioral merge gate.
//!
//! Default invocation runs the gate: read a diff against the base ref, check the
//! suite is green & stable, mutation-test the changed surface, run the
//! zero-assertion pre-check, render a verdict, and emit a validation record. In
//! gating mode (default) a blocked verdict exits non-zero so a required status
//! check fails; `--advisory` restores Phase 1 behaviour.
//!
//! Survivors are ranked by severity (Phase 4), and a debt-delta budget can gate
//! structural-debt growth. The `analyze` subcommand reads the collected
//! telemetry back into the Phase 3 KPIs plus the Phase 4 mutation-score trend
//! and severity mix.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};

use slop_filter::config::Config;
use slop_filter::estimate::Estimate;
use slop_filter::metrics::{self, RunContext, RunMetrics};
use slop_filter::report::GateReport;
use slop_filter::runner::RealRunner;
use slop_filter::severity::Severity;
use slop_filter::{analyze, diff, github, mutants, pipeline};

/// Exit code used when the gate blocks (distinct from operational failure).
const EXIT_BLOCKED: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Format {
    Text,
    Json,
    Markdown,
}

/// Differential mutation gate. Surfaces survivors the test suite passed over,
/// on the changed surface only, and blocks the merge when they exceed budget.
#[derive(Parser, Debug)]
#[command(name = "slop-gate", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    run: RunArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Summarise collected validation telemetry (JSON-Lines) into KPIs.
    Analyze(AnalyzeArgs),

    /// Project the mutant workload for a diff without building/testing.
    ///
    /// Runs only the enumerate + per-function cap steps (`cargo mutants
    /// --list`, no build), so it's fast and works on platforms where a full
    /// run can't. Use it to predict gate cost before pushing.
    Estimate(EstimateArgs),
}

#[derive(Args, Debug)]
struct AnalyzeArgs {
    /// Path to the JSON-Lines metrics file produced by `--metrics-file`.
    #[arg(long)]
    metrics_file: PathBuf,
}

#[derive(Args, Debug)]
struct EstimateArgs {
    /// Path to the repository under test.
    #[arg(long)]
    repo: Option<PathBuf>,

    /// Base ref to diff against (pass the merge-base for PR semantics).
    #[arg(long)]
    base: Option<String>,

    /// Head ref being estimated.
    #[arg(long)]
    head: Option<String>,

    /// Optional YAML config file (for the per-function cap, paths).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Hard cap on mutants tested per function (overrides config/default).
    #[arg(long)]
    max_per_function: Option<usize>,

    /// Emit JSON instead of the text table.
    #[arg(long)]
    json: bool,
}

impl EstimateArgs {
    /// Build the effective config from defaults → YAML → the estimate flags.
    fn to_config(&self) -> Result<Config> {
        let mut cfg = match &self.config {
            Some(path) => Config::from_yaml_file(path)?,
            None => Config::default(),
        };
        if let Some(v) = &self.repo {
            cfg.repo = v.clone();
        }
        if let Some(v) = &self.base {
            cfg.base_ref = v.clone();
        }
        if let Some(v) = &self.head {
            cfg.head_ref = v.clone();
        }
        if let Some(v) = self.max_per_function {
            cfg.max_mutants_per_function = v;
        }
        cfg.validate()?;
        Ok(cfg)
    }
}

/// Flags for the default gate run.
#[derive(Args, Debug)]
struct RunArgs {
    /// Path to the repository under test.
    #[arg(long)]
    repo: Option<PathBuf>,

    /// Base ref to diff against (pass the merge-base for PR semantics).
    #[arg(long)]
    base: Option<String>,

    /// Head ref being gated.
    #[arg(long)]
    head: Option<String>,

    /// Optional YAML config file. CLI flags override its values.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Parallel mutant jobs.
    #[arg(long)]
    jobs: Option<usize>,

    /// Per-mutant test timeout, in seconds.
    #[arg(long)]
    timeout: Option<u64>,

    /// Hard cap on mutants tested per function.
    #[arg(long)]
    max_per_function: Option<usize>,

    /// Survivors tolerated before blocking (0 = any survivor blocks).
    #[arg(long)]
    max_survivors: Option<usize>,

    /// How many times the suite runs in the determinism pre-flight.
    #[arg(long)]
    preflight_runs: Option<u32>,

    /// Skip the determinism pre-flight (e.g. CI already proved green).
    #[arg(long)]
    skip_preflight: bool,

    /// Narrow mutation tests to the changed crate only (faster, but a mutant
    /// caught only by a downstream crate's tests then shows as a survivor).
    /// Default runs the whole workspace's tests against each mutant.
    #[arg(long)]
    test_changed_package_only: bool,

    /// Test runner cargo-mutants drives: `cargo` (default) or `nextest`
    /// (per-process, highly parallel — often 2–3× faster; needs cargo-nextest).
    #[arg(long)]
    test_tool: Option<String>,

    /// Advisory mode: report but never block (Phase 1 behaviour).
    #[arg(long)]
    advisory: bool,

    /// Also block when zero-assertion tests are found.
    #[arg(long)]
    block_on_zero_assertion: bool,

    /// Block when a survivor reaches this severity tier, regardless of count
    /// (`low` | `medium` | `high` | `critical`). Advisory if unset.
    #[arg(long)]
    block_on_severity: Option<String>,

    /// Per-PR structural-debt budget (net complexity + duplication + coupling).
    #[arg(long)]
    debt_budget: Option<i64>,

    /// Block when the debt-delta exceeds the budget (advisory by default).
    #[arg(long)]
    block_on_debt: bool,

    /// Block when a pattern lane flags something. Repeatable / comma-separated;
    /// each value is a lane (`slop`|`security`|`convention`|`all`) or a rule id
    /// (e.g. `hardcoded-secret`, `unknown-crate-import`). Advisory if unset.
    #[arg(long, value_delimiter = ',')]
    block_on_pattern: Vec<String>,

    /// Post / update the report as a PR comment (uses the GitHub Actions env).
    #[arg(long)]
    comment: bool,

    /// Append a JSON-Lines run record to this file (validation telemetry).
    #[arg(long)]
    metrics_file: Option<PathBuf>,

    /// POST the run record to this telemetry endpoint (best-effort).
    #[arg(long)]
    metrics_url: Option<String>,

    /// Output format for the job log.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

impl RunArgs {
    /// Build the effective config: defaults → YAML file → CLI overrides.
    fn to_config(&self) -> Result<Config> {
        let mut cfg = match &self.config {
            Some(path) => Config::from_yaml_file(path)?,
            None => Config::default(),
        };
        if let Some(v) = &self.repo {
            cfg.repo = v.clone();
        }
        if let Some(v) = &self.base {
            cfg.base_ref = v.clone();
        }
        if let Some(v) = &self.head {
            cfg.head_ref = v.clone();
        }
        if let Some(v) = self.jobs {
            cfg.jobs = v;
        }
        if let Some(v) = self.timeout {
            cfg.timeout_secs = v;
        }
        if let Some(v) = self.max_per_function {
            cfg.max_mutants_per_function = v;
        }
        if let Some(v) = self.max_survivors {
            cfg.max_survivors = v;
        }
        if let Some(v) = self.preflight_runs {
            cfg.preflight_runs = v;
        }
        if self.test_changed_package_only {
            cfg.test_changed_package_only = true;
        }
        if let Some(v) = &self.test_tool {
            cfg.test_tool = v.clone();
        }
        if self.skip_preflight {
            cfg.skip_preflight = true;
        }
        if self.advisory {
            cfg.block_on_survivors = false;
            cfg.block_on_zero_assertion_tests = false;
        }
        if self.block_on_zero_assertion {
            cfg.block_on_zero_assertion_tests = true;
        }
        if let Some(v) = &self.metrics_file {
            cfg.metrics_file = Some(v.clone());
        }
        if let Some(v) = &self.metrics_url {
            cfg.metrics_url = Some(v.clone());
        }
        if let Some(v) = &self.block_on_severity {
            cfg.block_on_severity = Some(Severity::parse(v).with_context(|| {
                format!("invalid --block-on-severity `{v}` (expected low|medium|high|critical)")
            })?);
        }
        if let Some(v) = self.debt_budget {
            cfg.debt_budget = v;
        }
        if self.block_on_debt {
            cfg.block_on_debt = true;
        }
        if !self.block_on_pattern.is_empty() {
            cfg.block_on_pattern = self.block_on_pattern.clone();
        }
        cfg.validate()?;
        Ok(cfg)
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Analyze(args)) => match run_analyze(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("slop-gate: {e:#}");
                ExitCode::FAILURE
            }
        },
        Some(Command::Estimate(args)) => match run_estimate(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("slop-gate: {e:#}");
                ExitCode::FAILURE
            }
        },
        None => match run_gate(cli.run) {
            // A blocked verdict is a normal, reported outcome — exit 2 so a
            // required check fails, distinct from operational failure (1).
            Ok(report) if report.verdict.is_block() => ExitCode::from(EXIT_BLOCKED),
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("slop-gate: {e:#}");
                ExitCode::FAILURE
            }
        },
    }
}

fn run_gate(args: RunArgs) -> Result<GateReport> {
    let cfg = args.to_config()?;
    let want_comment = args.comment;
    let format = args.format;

    let work = tempfile::tempdir().context("creating work directory")?;
    let runner = RealRunner;
    let report = pipeline::run(&runner, &cfg, work.path())?;

    // PR comment is best-effort: a token/network hiccup shouldn't change the
    // gate's verdict or fail the step on its own.
    if want_comment {
        if let Err(e) = comment_on_pr(&report) {
            eprintln!("slop-gate: warning: could not post PR comment: {e:#}");
        }
    }

    // Validation telemetry is best-effort for the same reason.
    emit_metrics(&report, &cfg);

    match format {
        Format::Text => print!("{}", report.render_text()),
        Format::Json => println!("{}", report.render_json()),
        Format::Markdown => print!("{}", report.render_markdown()),
    }
    Ok(report)
}

fn run_analyze(args: &AnalyzeArgs) -> Result<()> {
    let runs = analyze::load(&args.metrics_file)?;
    print!("{}", analyze::summarize(&runs).render_text());
    Ok(())
}

/// Dry-run projection: enumerate candidates on the diff and apply the
/// per-function cap, without building or testing anything.
fn run_estimate(args: &EstimateArgs) -> Result<()> {
    let cfg = args.to_config()?;
    let work = tempfile::tempdir().context("creating work directory")?;
    let runner = RealRunner;

    let scope = diff::compute_scope(&cfg.repo, &cfg.base_ref, &cfg.head_ref, work.path())?;
    let candidates = if scope.changed_rust_files.is_empty() {
        Vec::new()
    } else {
        mutants::list_candidates(&runner, &cfg.repo, &scope.diff_path)?
    };
    let est = Estimate::from_candidates(&candidates, cfg.max_mutants_per_function);

    if args.json {
        println!("{}", est.render_json());
        // JSON consumers still need to know the estimate skipped Python — emit it on stderr so it
        // doesn't corrupt the machine-readable stdout.
        if !scope.changed_python_files.is_empty() {
            eprintln!(
                "slop-gate: warning: {} changed Python file(s) not estimated (cosmic-ray advisory PoC).",
                scope.changed_python_files.len()
            );
        }
    } else {
        print!("{}", est.render_text());
        if !scope.changed_python_files.is_empty() {
            println!(
                "note: {} changed Python file(s) not estimated (cosmic-ray advisory PoC).",
                scope.changed_python_files.len()
            );
        }
    }
    Ok(())
}

fn comment_on_pr(report: &GateReport) -> Result<()> {
    let ctx = github::GithubContext::from_env()?;
    github::post_or_update_comment(&ctx, &report.render_markdown())
}

fn emit_metrics(report: &GateReport, cfg: &Config) {
    if cfg.metrics_file.is_none() && cfg.metrics_url.is_none() {
        return;
    }
    let record = RunMetrics::from_report(report, cfg, &RunContext::from_env());
    if let Some(path) = &cfg.metrics_file {
        if let Err(e) = metrics::append_jsonl(path, &record) {
            eprintln!("slop-gate: warning: could not write metrics: {e:#}");
        }
    }
    if let Some(url) = &cfg.metrics_url {
        let token = std::env::var("METRICS_TOKEN").ok();
        if let Err(e) = metrics::post(url, token.as_deref(), &record) {
            eprintln!("slop-gate: warning: could not post metrics: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args() -> RunArgs {
        RunArgs {
            base: None,
            head: None,
            repo: None,
            config: None,
            jobs: None,
            timeout: None,
            max_per_function: None,
            max_survivors: None,
            preflight_runs: None,
            skip_preflight: false,
            test_changed_package_only: false,
            test_tool: None,
            advisory: false,
            block_on_zero_assertion: false,
            block_on_severity: None,
            block_on_pattern: vec![],
            debt_budget: None,
            block_on_debt: false,
            comment: false,
            metrics_file: None,
            metrics_url: None,
            format: Format::Text,
        }
    }

    #[test]
    fn to_config_propagates_base_ref() {
        // Guards the whole-function `Ok(Default::default())` mutation at
        // main.rs:211: a default Config has base_ref="" but this override
        // sets it to "main", so the mutation produces the wrong value.
        let args = RunArgs {
            base: Some("main".into()),
            ..base_args()
        };
        let cfg = args.to_config().unwrap();
        assert_eq!(cfg.base_ref, "main");
    }

    #[test]
    fn to_config_propagates_block_on_pattern() {
        // Guards the `!self.block_on_pattern.is_empty()` check at main.rs:266:
        // when `!` is deleted, the pattern list is only copied when empty, so
        // a non-empty list would never reach the config.
        let args = RunArgs {
            block_on_pattern: vec!["convention".into()],
            ..base_args()
        };
        let cfg = args.to_config().unwrap();
        assert_eq!(cfg.block_on_pattern, vec!["convention"]);
    }

    #[test]
    fn to_config_empty_block_on_pattern_leaves_config_default() {
        // Complement: an empty list must not overwrite a default (this also
        // confirms the `!is_empty()` guard is the right polarity).
        let args = base_args(); // block_on_pattern is empty
        let cfg = args.to_config().unwrap();
        assert!(cfg.block_on_pattern.is_empty());
    }
}
