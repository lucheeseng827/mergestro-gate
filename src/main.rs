// SPDX-License-Identifier: Apache-2.0
//! `slop-gate` — CLI for the Mergestro Gate behavioral merge gate.
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

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};

use mergestro_gate::config::Config;
use mergestro_gate::estimate::Estimate;
use mergestro_gate::metrics::{self, RunContext, RunMetrics};
use mergestro_gate::progression::init as progression_init;
use mergestro_gate::report::GateReport;
use mergestro_gate::runner::RealRunner;
use mergestro_gate::severity::Severity;
use mergestro_gate::{analyze, diff, github, mutants, pipeline};

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

    /// Build or refresh the turnover baseline (the maintainability lane's
    /// per-commit history file). Run once with full history, then commit or
    /// cache the file; the gate refreshes it incrementally on every run.
    Baseline(Box<BaselineArgs>),

    /// Project the mutant workload for a diff without building/testing.
    ///
    /// Runs only the enumerate + per-function cap steps (`cargo mutants
    /// --list`, no build), so it's fast and works on platforms where a full
    /// run can't. Use it to predict gate cost before pushing.
    Estimate(EstimateArgs),

    /// Resolve the repository's progression tree — an authored plan closed by
    /// the repo's own commits and PRs — and render it.
    ///
    /// Writes any of: the committed SVG (`--svg`), the README block between the
    /// `mergestro:progression` markers (`--readme`), and the JSON snapshot the
    /// Mergestro console ingests (`--json`). With `--check` nothing is written
    /// and a stale artifact exits 2, which is the CI assertion form.
    ///
    /// `progression init` scaffolds a first plan from the repository's own
    /// history, for repositories that do not have one yet.
    ///
    /// Boxed like `Baseline`: it carries the most flags of any subcommand, and
    /// an unboxed variant makes every `Command` the size of this one.
    Progression(Box<ProgressionArgs>),

    /// Combine the `--format json` reports of a sharded run (`--shard k/n`)
    /// into one: counts summed, the verdict recomputed under the flags given
    /// here, then printed / commented / written as SARIF once.
    MergeReports(Box<MergeArgs>),
}

#[derive(Args, Debug)]
struct MergeArgs {
    #[command(flatten)]
    run: RunArgs,

    /// The shard reports, one per shard (`slop-gate --shard k/n --format json`).
    #[arg(required = true)]
    reports: Vec<PathBuf>,
}

#[derive(Args, Debug)]
struct AnalyzeArgs {
    /// Path to the JSON-Lines metrics file produced by `--metrics-file`.
    #[arg(long)]
    metrics_file: PathBuf,
}

#[derive(Args, Debug)]
struct BaselineArgs {
    #[command(flatten)]
    run: RunArgs,

    /// Discard the existing baseline and walk from scratch.
    #[arg(long)]
    full: bool,

    /// First run only: ignore commits older than this many days.
    #[arg(long)]
    since_days: Option<u32>,
}

#[derive(Args, Debug)]
// `progression init` takes none of the resolve flags, and passing both means
// the caller expected one of them to do something. Say so rather than picking.
#[command(args_conflicts_with_subcommands = true)]
struct ProgressionArgs {
    #[command(subcommand)]
    command: Option<ProgressionCommand>,

    /// The authored plan (YAML). See `mergestro-progression.example.yaml`.
    ///
    /// Optional only so that `progression init` — which writes one — can run
    /// without it; resolving still requires it.
    #[arg(long)]
    spec: Option<PathBuf>,

    /// Repository to resolve the plan against.
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// Ref whose ancestry is the history. Defaults to `HEAD`.
    #[arg(long, default_value = "HEAD")]
    head: String,

    /// Ignore commits older than this many days (overrides the spec).
    #[arg(long)]
    since_days: Option<u32>,

    /// Write the JSON snapshot here.
    #[arg(long)]
    json: Option<PathBuf>,

    /// Write the SVG drawing here.
    #[arg(long)]
    svg: Option<PathBuf>,

    /// Write the Markdown block here (standalone, not injected).
    #[arg(long)]
    markdown: Option<PathBuf>,

    /// Update the block between the `mergestro:progression` markers in this file.
    #[arg(long)]
    readme: Option<PathBuf>,

    /// `src` for the README's `<img>`. Defaults to `--svg` made relative to the
    /// README's directory.
    #[arg(long)]
    svg_href: Option<String>,

    /// Assert instead of write: exit 2 when any requested artifact is stale.
    #[arg(long)]
    check: bool,

    /// Stop the history walk after this many commits (default 20000).
    ///
    /// A guard against an unbounded walk of a decade-old monorepo, not a feature. Raise it when
    /// the walk reports it hit the cap — the alternative is a tree that understates every
    /// milestone older than the cap.
    #[arg(long)]
    max_commits: Option<usize>,

    /// Resolve against a truncated history anyway.
    ///
    /// A shallow checkout (`actions/checkout`'s default) can only see the last
    /// few commits, so every milestone reads as barely started — wrong, and
    /// indistinguishable from real regression. The walk refuses by default; use
    /// `fetch-depth: 0` instead of this flag wherever the artifact is committed.
    #[arg(long)]
    allow_shallow: bool,

    /// POST the snapshot to a Mergestro ingest endpoint (best-effort).
    /// Token: `METRICS_TOKEN`, same as the gate's telemetry upload.
    #[arg(long)]
    plane_url: Option<String>,

    /// `owner/repo` the snapshot is filed under in the plane. Defaults to
    /// `GITHUB_REPOSITORY`.
    #[arg(long)]
    slug: Option<String>,

    /// Output format for the job log.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

#[derive(Subcommand, Debug)]
enum ProgressionCommand {
    /// Scaffold a first plan from the repository's own history.
    ///
    /// Mines the components the repository actually has and the marker its
    /// merges write — the two things a hand-written first spec gets wrong
    /// silently — and leaves one milestone open for the work you are planning.
    /// The draft is a starting point to edit, not a plan: history can say what
    /// a repository has done, never what it meant to do.
    Init(ProgressionInitArgs),
}

#[derive(Args, Debug)]
struct ProgressionInitArgs {
    /// Repository to mine.
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// Where to write the draft.
    #[arg(long, default_value = "progression.yaml")]
    out: PathBuf,

    /// Ref whose ancestry to mine.
    #[arg(long, default_value = "HEAD")]
    head: String,

    /// Plan title. Defaults to the repository's directory name.
    #[arg(long)]
    title: Option<String>,

    /// Period label written into the draft ("2026 H1", "Sprint 14").
    #[arg(long)]
    season: Option<String>,

    /// Mine only the last N days, and write that window into the draft.
    ///
    /// Makes the tree a sliding window: a milestone closed by commits that
    /// later fall out of it re-opens. Right for a sprint tree, wrong for a plan
    /// meant to stay closed.
    #[arg(long)]
    since_days: Option<u32>,

    /// At most this many mined milestones.
    #[arg(long, default_value_t = progression_init::DEFAULT_MAX_NODES)]
    max_nodes: usize,

    /// A directory needs this many commits to become a milestone.
    #[arg(long, default_value_t = progression_init::DEFAULT_MIN_COMMITS)]
    min_commits: u32,

    /// How many path segments deep a component may sit (`a/b/c` is 3).
    #[arg(long, default_value_t = progression_init::DEFAULT_DEPTH)]
    depth: usize,

    /// Stop the history walk after this many commits.
    #[arg(long)]
    max_commits: Option<usize>,

    /// Print the draft instead of writing it.
    #[arg(long)]
    stdout: bool,

    /// Overwrite an existing file.
    #[arg(long)]
    force: bool,
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

    /// Kept for compatibility: narrowing mutation tests to the changed crate is
    /// the default since 0.6.0, so this flag no longer changes anything.
    #[arg(long, conflicts_with = "test_workspace")]
    test_changed_package_only: bool,

    /// Run the whole workspace's tests against each mutant, not only the changed
    /// crate's (slower; catches a mutant only a downstream crate's tests notice).
    #[arg(long)]
    test_workspace: bool,

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

    /// Wall-clock limit on the Rust mutation run: `600`, `90s`, `10m`, `1h`.
    /// Mutants still running at the limit are reported as not tested.
    #[arg(long, value_parser = parse_duration_secs)]
    budget: Option<u64>,

    /// Block when the budget left mutants untested (a warning by default).
    #[arg(long)]
    block_on_budget: bool,

    /// Run one shard of the Rust mutants, `k/n` (e.g. `2/4`), for a CI matrix;
    /// combine the shards' `--format json` reports with `merge-reports`.
    #[arg(long)]
    shard: Option<String>,

    /// Mutate the checkout itself instead of a scratch copy, reusing the
    /// pre-flight's build (and a cached target/) instead of a second cold build.
    /// One mutant at a time (`--jobs` is ignored). For CI checkouts.
    #[arg(long)]
    in_place: bool,

    /// Block when a pattern lane flags something. Repeatable / comma-separated;
    /// each value is a lane (`slop`|`security`|`convention`|`all`) or a rule id
    /// (e.g. `hardcoded-secret`, `unknown-crate-import`). Advisory if unset.
    #[arg(long, value_delimiter = ',')]
    block_on_pattern: Vec<String>,

    /// How strict the MCP lane is: `never` | `critical` | `unproven` | `any`.
    /// Only meaningful when the config declares `mcp_servers`.
    #[arg(long)]
    mcp_fail_on: Option<String>,

    /// Path to the `specprobe` binary the MCP lane drives (default: on PATH).
    #[arg(long)]
    specprobe_bin: Option<PathBuf>,

    /// Post / update the report as a PR comment (uses the GitHub Actions env).
    #[arg(long)]
    comment: bool,

    /// Also post each surviving mutant without an inline comment yet as a review
    /// comment on its line (GitHub only). Needs `--comment`: the summary
    /// comment carries the state that keeps reruns from repeating them.
    #[arg(long, requires = "comment")]
    comment_inline: bool,

    /// Disable the turnover (maintainability drift) lane for this run.
    #[arg(long)]
    no_turnover: bool,

    /// Turnover baseline file (default: .turnover/baseline.json in the repo).
    #[arg(long)]
    turnover_baseline: Option<PathBuf>,

    /// Block when the change drifts past the turnover policy (advisory by default).
    #[arg(long)]
    block_on_turnover_drift: bool,

    /// Mergestro plane URL for the turnover baseline service (fetch before, push after
    /// with `turnover.update_baseline`). Token: METRICS_TOKEN.
    #[arg(long)]
    turnover_baseline_url: Option<String>,

    /// Append a JSON-Lines run record to this file (validation telemetry).
    #[arg(long)]
    metrics_file: Option<PathBuf>,

    /// POST the run record to this telemetry endpoint (best-effort).
    #[arg(long)]
    metrics_url: Option<String>,

    /// Also write the findings as SARIF 2.1.0 to this path, for GitHub code
    /// scanning (`github/codeql-action/upload-sarif`). Independent of `--format`.
    #[arg(long)]
    sarif: Option<PathBuf>,

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
        if self.test_workspace {
            cfg.test_changed_package_only = false;
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
            // Advisory means advisory everywhere. The MCP lane gates by default
            // once a server is declared, so leaving it out here would make
            // `--advisory` a half-truth on exactly the repos that adopt it.
            cfg.mcp_fail_on = "never".to_string();
            // A YAML `block_on_budget: true` too; an explicit --block-on-budget
            // still wins, applied below like the other explicit block flags.
            cfg.block_on_budget = false;
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
        if let Some(v) = self.budget {
            cfg.budget_secs = Some(v);
        }
        if self.block_on_budget {
            cfg.block_on_budget = true;
        }
        if self.in_place {
            cfg.in_place = true;
        }
        if let Some(v) = &self.shard {
            cfg.shard = Some(v.clone());
        }
        if self.no_turnover {
            cfg.turnover.enabled = false;
        }
        if let Some(v) = &self.turnover_baseline {
            cfg.turnover.baseline = v.clone();
        }
        if self.block_on_turnover_drift {
            cfg.turnover.block_on_drift = true;
        }
        if let Some(v) = &self.turnover_baseline_url {
            cfg.turnover.baseline_url = Some(v.clone());
        }
        if !self.block_on_pattern.is_empty() {
            cfg.block_on_pattern = self.block_on_pattern.clone();
        }
        // After --advisory, so an explicit --mcp-fail-on still wins: asking for
        // a specific threshold is a narrower instruction than "advisory".
        if let Some(v) = &self.mcp_fail_on {
            cfg.mcp_fail_on = v.clone();
        }
        if let Some(v) = &self.specprobe_bin {
            cfg.specprobe_bin = v.to_string_lossy().into_owned();
        }
        cfg.validate()?;
        Ok(cfg)
    }
}

/// Parse `--budget`: whole seconds, optionally suffixed `s`, `m` or `h`.
fn parse_duration_secs(raw: &str) -> Result<u64, String> {
    let raw = raw.trim();
    let (digits, unit) = match raw.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => (&raw[..i], c.to_ascii_lowercase()),
        _ => (raw, 's'),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("`{raw}` is not a duration (use e.g. 600, 90s, 10m, 1h)"))?;
    let secs = match unit {
        's' => n,
        'm' => n.saturating_mul(60),
        'h' => n.saturating_mul(3600),
        _ => return Err(format!("`{raw}`: unknown unit (use s, m or h)")),
    };
    if secs == 0 {
        return Err("the budget must be at least 1 second".into());
    }
    Ok(secs)
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
        Some(Command::Baseline(args)) => match run_baseline(&args) {
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
        Some(Command::Progression(args)) => match run_progression(&args) {
            // A stale committed artifact is a reported outcome, not a crash —
            // exit 2, the same code a blocked gate uses, so `--check` can be a
            // required status check.
            Ok(stale) if stale => ExitCode::from(EXIT_BLOCKED),
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("slop-gate: {e:#}");
                ExitCode::FAILURE
            }
        },
        Some(Command::MergeReports(args)) => ExitCode::from(verdict_exit(run_merge(&args))),
        None => ExitCode::from(verdict_exit(run_gate(cli.run))),
    }
}

/// The exit code for a run that produced a verdict. A blocked verdict is a
/// normal, reported outcome — 2, so a required check fails — distinct from an
/// operational failure (1).
fn verdict_exit(result: Result<GateReport>) -> u8 {
    match result {
        Ok(report) if report.verdict.is_block() => EXIT_BLOCKED,
        Ok(_) => 0,
        Err(e) => {
            eprintln!("slop-gate: {e:#}");
            1
        }
    }
}

fn run_gate(args: RunArgs) -> Result<GateReport> {
    let cfg = args.to_config()?;
    // One shard's survivors are not the PR's: shards commenting would take turns
    // overwriting the one summary (and its survivor / inline state). Only the
    // merged report is published. The effective config, not the flag: a YAML
    // file can set `shard` too.
    anyhow::ensure!(
        cfg.shard.is_none() || !args.comment,
        "a sharded run (--shard or `shard:` in the config) cannot --comment: write \
         --format json and publish once with `slop-gate merge-reports --comment`"
    );
    let want_comment = args.comment;
    let format = args.format;

    let work = tempfile::tempdir().context("creating work directory")?;
    let runner = RealRunner;
    let report = pipeline::run(&runner, &cfg, work.path())?;

    // PR comment is best-effort: a token/network hiccup shouldn't change the
    // gate's verdict or fail the step on its own.
    if want_comment {
        // Inline comments may only land on lines the diff shows; read it while
        // the work directory still exists.
        let diff = args
            .comment_inline
            .then(|| std::fs::read_to_string(work.path().join("changed.diff")).unwrap_or_default());
        if let Err(e) = comment_on_pr(&report, diff.as_deref()) {
            eprintln!("slop-gate: warning: could not post PR comment: {e:#}");
        }
    }

    // Validation telemetry is best-effort for the same reason.
    emit_metrics(&report, &cfg);

    emit_report(&report, args.sarif.as_deref(), format)?;
    Ok(report)
}

/// Write `--sarif` and print the report in `format`: the tail of a gate run,
/// shared with `merge-reports`.
fn emit_report(report: &GateReport, sarif: Option<&Path>, format: Format) -> Result<()> {
    // Not best-effort: a SARIF path was asked for, and an upload step after
    // this one would otherwise fail on a missing file, or publish a stale one.
    if let Some(path) = sarif {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(path, mergestro_gate::sarif::render(report))
            .with_context(|| format!("writing SARIF to {}", path.display()))?;
    }
    match format {
        Format::Text => print!("{}", report.render_text()),
        Format::Json => println!("{}", report.render_json()),
        Format::Markdown => print!("{}", report.render_markdown()),
    }
    Ok(())
}

/// Combine the `--format json` reports of one sharded run, recompute the
/// verdict under this run's flags (a `--max-survivors` budget applies to the
/// total, not to each shard), and publish it once.
fn run_merge(args: &MergeArgs) -> Result<GateReport> {
    // Inline comments may only land on lines the diff shows, and a merge job has
    // no diff. Refuse the flag rather than accept it and quietly post none.
    anyhow::ensure!(
        !args.run.comment_inline,
        "merge-reports does not post inline review comments (it has no diff to place them on); \
         drop --comment-inline, or use it on an unsharded run"
    );
    let cfg = args.run.to_config()?;
    let reports = args
        .reports
        .iter()
        .map(|p| {
            let text =
                std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("{} is not a `--format json` gate report", p.display()))
        })
        .collect::<Result<Vec<GateReport>>>()?;
    let mut report = mergestro_gate::report::merge(reports)?;
    report.verdict = mergestro_gate::verdict::decide(&report, &cfg);
    if args.run.comment {
        if let Err(e) = comment_on_pr(&report, None) {
            eprintln!("slop-gate: warning: could not post PR comment: {e:#}");
        }
    }
    emit_report(&report, args.run.sarif.as_deref(), args.run.format)?;
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
        let packages = mutants::changed_packages(&cfg.repo, &scope.changed_rust_files);
        mutants::list_candidates(&runner, &cfg.repo, &scope.diff_path, &packages)?
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

/// Post the summary comment and, with `inline_diff`, a review of the survivors
/// that have no inline comment yet.
fn comment_on_pr(report: &GateReport, inline_diff: Option<&str>) -> Result<()> {
    github::publish(&github::GithubContext::from_env()?, report, inline_diff)
}

fn emit_metrics(report: &GateReport, cfg: &Config) {
    if cfg.metrics_file.is_none() && cfg.metrics_url.is_none() {
        return;
    }
    let ctx = RunContext::from_env();
    let record = RunMetrics::from_report(report, cfg, &ctx);
    // The slop record plus, when the turnover lane measured something, its own record: two
    // lines on one wire, so the control plane sees the mutation verdict and the drift for the
    // same head in one batch.
    let mut lines = vec![record.to_json_line()];
    if let Some(line) = metrics::turnover_record_line(report, &ctx) {
        lines.push(line);
    }
    if let Some(path) = &cfg.metrics_file {
        for line in &lines {
            if let Err(e) = metrics::append_line(path, line) {
                eprintln!("slop-gate: warning: could not write metrics: {e:#}");
                break;
            }
        }
    }
    if let Some(url) = &cfg.metrics_url {
        let token = std::env::var("METRICS_TOKEN").ok();
        if let Err(e) = metrics::post_lines(url, token.as_deref(), &lines) {
            eprintln!("slop-gate: warning: could not post metrics: {e:#}");
        }
    }
}

/// `slop-gate progression` — resolve the plan against history and render it.
///
/// Returns `true` when `--check` found something stale, which the caller turns
/// into exit 2. Writing and checking share one code path on purpose: the check
/// compares against exactly the bytes the write would have produced, so a green
/// check is a guarantee that running without `--check` changes nothing.
fn run_progression(args: &ProgressionArgs) -> Result<bool> {
    use mergestro_gate::progression::{self, render};

    if let Some(ProgressionCommand::Init(init)) = &args.command {
        return run_progression_init(init).map(|_| false);
    }

    let spec_path = args.spec.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "progression needs a plan: pass --spec <file>, or run `slop-gate progression init` \
             to scaffold one from this repository's history"
        )
    })?;
    let mut spec = progression::ProgressionSpec::from_yaml_file(spec_path)?;
    if let Some(days) = args.since_days {
        spec.since_days = Some(days);
    }
    let now = progression::now_unix();
    let (snap, stats) =
        progression::snapshot(&args.repo, &spec, &args.head, now, args.max_commits)?;
    if stats.truncated {
        // Not a hard refusal like a shallow clone: the cap is a deliberate guard that only bites
        // enormous repositories, and `--max-commits` is the remedy. But it must never be silent —
        // it understates exactly the way a shallow walk does.
        eprintln!(
            "slop-gate: warning: the walk stopped at {} commits with history left to read; \
             milestones older than that are understated. Raise --max-commits.",
            stats.commits
        );
    }
    if stats.shallow_boundary > 0 {
        if !args.allow_shallow {
            anyhow::bail!(
                "history is truncated at a shallow boundary ({} commit(s) unreadable, {} read) — \
                 the tree would understate every milestone. Check out with `fetch-depth: 0`, or \
                 pass --allow-shallow to resolve against what is here.",
                stats.shallow_boundary,
                stats.commits
            );
        }
        eprintln!(
            "slop-gate: warning: shallow history — {} commit(s) unreadable; milestones are understated.",
            stats.shallow_boundary
        );
    }

    // The README's `<img src>` is relative to the README, not to the CWD the
    // job happens to run in.
    let href = args.svg_href.clone().or_else(|| {
        let svg = args.svg.as_ref()?;
        let readme = args.readme.as_ref()?;
        Some(relative_href(readme, svg))
    });

    let mut stale = Vec::new();
    if let Some(path) = &args.svg {
        stale.extend(write_or_check(
            path,
            &render::render_svg(&snap),
            args.check,
        )?);
    }
    if let Some(path) = &args.json {
        stale.extend(write_or_check(path, &snap.render_json(), args.check)?);
    }
    if let Some(path) = &args.markdown {
        let md = render::render_markdown(&snap, href.as_deref());
        stale.extend(write_or_check(path, &md, args.check)?);
    }
    if let Some(path) = &args.readme {
        let current =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let block = render::render_markdown(&snap, href.as_deref());
        let updated = render::inject_readme(&current, &block)?;
        stale.extend(write_or_check(path, &updated, args.check)?);
    }

    // `--check` is an assertion, not a publish: a dry run must not leave a record behind on a
    // service the caller only meant to compare against.
    if let Some(url) = args.plane_url.as_ref().filter(|_| !args.check) {
        post_progression(&snap, args, url);
    }

    match args.format {
        Format::Text => print!("{}", render::render_text(&snap)),
        Format::Json => print!("{}", snap.render_json()),
        Format::Markdown => print!("{}", render::render_markdown(&snap, href.as_deref())),
    }

    if !stale.is_empty() {
        eprintln!(
            "slop-gate: progression is stale — re-run without --check and commit: {}",
            stale.join(", ")
        );
        return Ok(true);
    }
    Ok(false)
}

/// `slop-gate progression init` — mine a first draft of the plan.
///
/// Walks once and uses the commits three times: to mine components, to detect
/// the merge marker, and to resolve the draft it just wrote. That last one is
/// the point of the report it prints — a scaffolded plan for an established
/// repository reads as ~100% done, and an author who is not told that will read
/// their first tree as a finished quarter.
fn run_progression_init(args: &ProgressionInitArgs) -> Result<()> {
    use mergestro_gate::progression::{self, history, init, resolve, spec};

    if args.out.exists() && !args.force && !args.stdout {
        anyhow::bail!(
            "{} already exists — pass --force to overwrite it, --out to write elsewhere, or \
             --stdout to print the draft",
            args.out.display()
        );
    }

    let now = progression::now_unix();
    // The marker this repository writes is what init is trying to LEARN, so the
    // walk parses PR numbers with the default pattern and the scaffolder reads
    // the subjects itself.
    let pr_pattern =
        regex::Regex::new(spec::DEFAULT_PR_PATTERN).context("compiling the default PR pattern")?;
    let walk = history::WalkOptions {
        head: args.head.clone(),
        since_unix: args
            .since_days
            .map(|d| now.saturating_sub(d as u64 * 86_400)),
        max_commits: args
            .max_commits
            .unwrap_or_else(|| history::WalkOptions::default().max_commits),
    };
    let hist = history::walk(&args.repo, &walk, &pr_pattern)?;

    // Unlike a resolve, a shallow or capped walk is not fatal here: a draft is
    // a draft. It does change what can be claimed, so it is said out loud and
    // `init::draft` drops the phases it can no longer order.
    if hist.stats.shallow_boundary > 0 {
        eprintln!(
            "slop-gate: warning: shallow history ({} commit(s) unreadable) — the draft only knows \
             the components this checkout can see. `fetch-depth: 0` sees all of them.",
            hist.stats.shallow_boundary
        );
    }
    if hist.stats.truncated {
        eprintln!(
            "slop-gate: warning: the walk stopped at {} commits with history left to read; raise \
             --max-commits for an older repository.",
            hist.stats.commits
        );
    }

    let opts = init::InitOptions {
        title: args
            .title
            .clone()
            .unwrap_or_else(|| default_plan_title(&args.repo)),
        season: args.season.clone(),
        max_nodes: args.max_nodes,
        min_commits: args.min_commits,
        depth: args.depth,
        since_days: args.since_days,
    };
    let draft = init::draft(&hist.commits, &hist.stats, &opts, now)?;

    if args.stdout {
        print!("{}", draft.yaml);
    } else {
        if let Some(parent) = args.out.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        std::fs::write(&args.out, &draft.yaml)
            .with_context(|| format!("writing {}", args.out.display()))?;
        eprintln!("slop-gate: wrote {}", args.out.display());
    }

    for note in &draft.notes {
        eprintln!("  {note}");
    }
    for component in &draft.components {
        eprintln!(
            "  · {} — {} commits, {} files",
            component.path, component.commits, component.files
        );
    }

    // What the draft says today, said by the same resolver CI will use.
    let head_sha = progression::resolve_head_sha(&args.repo, &args.head)?;
    let snap = resolve::resolve(&draft.spec, &hist.commits, &head_sha, now)?;
    eprintln!(
        "  reads now: {}/{} milestones done · level {} · {}%",
        snap.totals.nodes_done,
        snap.totals.nodes_total,
        snap.totals.level,
        snap.totals.pct_bp / 100,
    );
    match &draft.frontier_id {
        Some(id) => eprintln!(
            "  the milestones above `{id}` describe work that already landed — `{id}` is the one \
             you write"
        ),
        None => eprintln!("  edit the globs to your layout, then the titles to the work"),
    }
    if !args.stdout {
        eprintln!(
            "  next: slop-gate progression --spec {} --svg docs/progression.svg --readme README.md",
            args.out.display()
        );
    }
    Ok(())
}

/// A plan title for a repository that was not given one: its directory name.
fn default_plan_title(repo: &Path) -> String {
    std::fs::canonicalize(repo)
        .ok()
        .and_then(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| !n.is_empty())
        })
        .map(|name| format!("{name} — progression"))
        .unwrap_or_else(|| "Progression".to_string())
}

/// Write `content` to `path`, or (in check mode) report the path when it differs.
///
/// Also reports "would change" for a file that does not exist yet: a check that
/// passed because the artifact was never generated is the failure this guards.
fn write_or_check(path: &PathBuf, content: &str, check: bool) -> Result<Vec<String>> {
    let current = std::fs::read_to_string(path).ok();
    if current.as_deref() == Some(content) {
        return Ok(Vec::new());
    }
    if check {
        return Ok(vec![path.display().to_string()]);
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(Vec::new())
}

/// `svg` expressed relative to the directory holding `readme`.
///
/// A browser resolves the `<img src>` against the README's own directory, so
/// anything but a true relative path is a broken image. `strip_prefix` alone
/// only covers the case where the SVG sits *under* the README's directory; for
/// a sibling (`docs/README.md` + `assets/p.svg`) it fails, and returning the
/// path as given would resolve to `docs/assets/p.svg`. So walk off the
/// non-shared part of the README's directory as `..` and append the rest.
fn relative_href(readme: &Path, svg: &Path) -> String {
    use std::path::Component;
    let dir = readme.parent().unwrap_or(Path::new(""));
    let keep = |c: &Component<'_>| matches!(c, Component::Normal(_));
    let from: Vec<Component<'_>> = dir.components().filter(keep).collect();
    let to: Vec<Component<'_>> = svg.components().filter(keep).collect();

    let shared = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_string(); from.len() - shared];
    parts.extend(
        to[shared..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    if parts.is_empty() {
        // The SVG *is* the README's directory — nonsense input, but a caller
        // gets its own path back rather than an empty `src`.
        return svg.to_string_lossy().replace('\\', "/");
    }
    parts.join("/")
}

/// Best-effort upload of the snapshot to a Mergestro plane. Never fails the
/// command: a docs refresh must not go red because telemetry was unreachable.
fn post_progression(
    snap: &mergestro_gate::progression::ProgressionSnapshot,
    args: &ProgressionArgs,
    url: &str,
) {
    use mergestro_gate::progression::record;

    let ctx = RunContext::from_env();
    let Some(repo) = args.slug.clone().or_else(|| ctx.repo.clone()) else {
        eprintln!(
            "slop-gate: warning: --plane-url needs a repo slug (pass --slug or set GITHUB_REPOSITORY)"
        );
        return;
    };
    let identity = record::Identity {
        repo,
        pr: ctx.pr,
        head_sha: ctx.head_sha.clone().unwrap_or_default(),
        run_id: ctx
            .run_id
            .clone()
            .unwrap_or_else(|| format!("local-{}", ctx.timestamp_unix)),
        actor: ctx.actor.clone(),
        pr_author: ctx.pr_author.clone(),
        timestamp_unix: ctx.timestamp_unix,
    };
    let line = record::to_json_line(&record::build(identity, snap));
    let token = std::env::var("METRICS_TOKEN").ok();
    if let Err(e) = metrics::post_lines(url, token.as_deref(), &[line]) {
        eprintln!("slop-gate: warning: could not post progression snapshot: {e:#}");
    }
}

/// `slop-gate baseline` — build or refresh the turnover baseline.
fn run_baseline(args: &BaselineArgs) -> Result<()> {
    let cfg = args.run.to_config()?;
    let summary = mergestro_gate::turnover_lane::build_baseline(&cfg, args.full, args.since_days)?;
    eprintln!(
        "slop-gate: turnover baseline {} — {} new commits classified, {} total ({:.1}s)",
        summary.path.display(),
        summary.new_commits,
        summary.total_commits,
        summary.elapsed_secs
    );
    if let Some(all) = &summary.whole_history {
        eprintln!(
            "slop-gate: whole-history ratios — {}",
            turnover_gate::render::ratio_line(all)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_definition_is_well_formed() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn progression_keeps_its_flags_and_init_takes_none_of_them() {
        use clap::Parser;

        // The refresh workflow on `main` runs exactly this shape. Adding a
        // nested subcommand must not have moved it.
        let cli = Cli::try_parse_from([
            "slop-gate",
            "progression",
            "--spec",
            "p.yaml",
            "--svg",
            "a.svg",
        ])
        .expect("resolving still parses");
        let Some(Command::Progression(args)) = cli.command else {
            panic!("expected the progression command");
        };
        assert!(args.command.is_none());
        assert_eq!(args.spec.as_deref(), Some(Path::new("p.yaml")));

        let cli = Cli::try_parse_from(["slop-gate", "progression", "init", "--out", "p.yaml"])
            .expect("init parses without --spec");
        let Some(Command::Progression(args)) = cli.command else {
            panic!("expected the progression command");
        };
        let Some(ProgressionCommand::Init(init)) = args.command else {
            panic!("expected the init subcommand");
        };
        assert_eq!(init.out, PathBuf::from("p.yaml"));

        // Both at once means one of them was expected to do something.
        assert!(
            Cli::try_parse_from(["slop-gate", "progression", "--spec", "p.yaml", "init"]).is_err()
        );
    }

    #[test]
    fn resolving_without_a_spec_says_how_to_get_one() {
        let args = ProgressionArgs {
            command: None,
            spec: None,
            repo: PathBuf::from("."),
            head: "HEAD".into(),
            since_days: None,
            json: None,
            svg: None,
            markdown: None,
            readme: None,
            svg_href: None,
            check: false,
            max_commits: None,
            allow_shallow: false,
            plane_url: None,
            slug: None,
            format: Format::Text,
        };
        let err = run_progression(&args).unwrap_err().to_string();
        assert!(err.contains("progression init"), "{err}");
        // A wrapped literal that lost its `\` continuation reads as a run of
        // spaces in the terminal, and every test that only greps for a substring
        // passes anyway. This is what catches it.
        assert!(!err.contains("  "), "double space in the message: {err}");
    }

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
            test_workspace: false,
            test_tool: None,
            advisory: false,
            block_on_zero_assertion: false,
            block_on_severity: None,
            block_on_pattern: vec![],
            debt_budget: None,
            block_on_debt: false,
            budget: None,
            block_on_budget: false,
            in_place: false,
            shard: None,
            comment: false,
            comment_inline: false,
            no_turnover: false,
            turnover_baseline: None,
            block_on_turnover_drift: false,
            turnover_baseline_url: None,
            metrics_file: None,
            metrics_url: None,
            mcp_fail_on: None,
            specprobe_bin: None,
            sarif: None,
            format: Format::Text,
        }
    }

    #[test]
    fn a_readme_image_href_is_relative_to_the_readme() {
        // The job's CWD is irrelevant — the browser resolves the `src` against the README.
        assert_eq!(
            relative_href(Path::new("mod/README.md"), Path::new("mod/docs/p.svg")),
            "docs/p.svg"
        );
        // A README at the repo root and an SVG beside it.
        assert_eq!(
            relative_href(Path::new("README.md"), Path::new("docs/p.svg")),
            "docs/p.svg"
        );
        // Siblings: the README's directory has to be walked off, or the browser resolves the
        // image under it. This test used to assert `b/p.svg` — i.e. a broken image.
        assert_eq!(
            relative_href(Path::new("a/README.md"), Path::new("b/p.svg")),
            "../b/p.svg"
        );
        assert_eq!(
            relative_href(
                Path::new("docs/README.md"),
                Path::new("assets/progression.svg")
            ),
            "../assets/progression.svg"
        );
        // Two levels up, and a partially shared prefix that must not be over-consumed.
        assert_eq!(
            relative_href(Path::new("a/b/c/README.md"), Path::new("a/x/p.svg")),
            "../../x/p.svg"
        );
        // The real monorepo shape stays a plain descent.
        assert_eq!(
            relative_href(
                Path::new("./README.md"),
                Path::new("./docs/progression.svg")
            ),
            "docs/progression.svg"
        );
    }

    #[test]
    fn check_mode_reports_a_missing_artifact_instead_of_passing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("p.svg");

        // A check that passed because the file was never generated is the failure this guards.
        let stale = write_or_check(&path, "body", true).unwrap();
        assert_eq!(stale.len(), 1);
        assert!(!path.exists(), "check mode must not write");

        // Writing creates the parent directory, and re-writing the same bytes is not stale.
        assert!(write_or_check(&path, "body", false).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "body");
        assert!(write_or_check(&path, "body", true).unwrap().is_empty());
        assert_eq!(write_or_check(&path, "other", true).unwrap().len(), 1);
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
    fn a_shard_run_may_not_comment() {
        let args = RunArgs {
            shard: Some("1/2".into()),
            comment: true,
            ..base_args()
        };
        let err = run_gate(args).unwrap_err().to_string();
        assert!(err.contains("merge-reports --comment"), "{err}");

        // The same from a config file, with no --shard on the command line.
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("gate.yaml");
        std::fs::write(&yaml, "shard: \"2/2\"\n").unwrap();
        let from_yaml = RunArgs {
            config: Some(yaml),
            comment: true,
            ..base_args()
        };
        let err = run_gate(from_yaml).unwrap_err().to_string();
        assert!(err.contains("merge-reports --comment"), "{err}");
    }

    #[test]
    fn a_blocked_verdict_exits_2_and_an_operational_failure_1() {
        use mergestro_gate::report::Verdict;
        let pass = GateReport::new("main", "HEAD");
        let mut blocked = pass.clone();
        blocked.verdict = Verdict::Block {
            reasons: vec!["survivors".into()],
        };
        assert_eq!(verdict_exit(Ok(pass)), 0);
        assert_eq!(verdict_exit(Ok(blocked)), EXIT_BLOCKED);
        assert_eq!(verdict_exit(Err(anyhow::anyhow!("no git repo"))), 1);
    }

    #[test]
    fn emit_report_writes_the_sarif_it_was_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out/nested/mergestro.sarif");
        emit_report(&GateReport::new("main", "HEAD"), Some(&path), Format::Json).unwrap();
        let sarif: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(sarif["version"], "2.1.0");
    }

    #[test]
    fn budget_parses_the_common_spellings_and_refuses_the_rest() {
        assert_eq!(parse_duration_secs("600"), Ok(600));
        assert_eq!(parse_duration_secs("90s"), Ok(90));
        assert_eq!(parse_duration_secs("10m"), Ok(600));
        assert_eq!(parse_duration_secs("1H"), Ok(3600));
        for bad in ["", "m", "0", "0m", "10d", "ten", "-5", "1.5m"] {
            assert!(parse_duration_secs(bad).is_err(), "should refuse `{bad}`");
        }
    }

    #[test]
    fn advisory_turns_budget_blocking_off_unless_asked_explicitly() {
        let advisory = RunArgs {
            advisory: true,
            budget: Some(60),
            ..base_args()
        };
        let cfg = advisory.to_config().unwrap();
        assert_eq!(cfg.budget_secs, Some(60));
        assert!(!cfg.block_on_budget);

        let explicit = RunArgs {
            advisory: true,
            block_on_budget: true,
            ..base_args()
        };
        assert!(explicit.to_config().unwrap().block_on_budget);
    }

    #[test]
    fn test_scope_defaults_to_the_changed_crate_and_widens_on_request() {
        // 0.6.0: the fast path is the default; `--test-workspace` is the way back.
        assert!(base_args().to_config().unwrap().test_changed_package_only);
        let widened = RunArgs {
            test_workspace: true,
            ..base_args()
        };
        assert!(!widened.to_config().unwrap().test_changed_package_only);

        // The old opt-in flag still parses, but asking for both is a contradiction.
        use clap::Parser;
        assert!(Cli::try_parse_from(["slop-gate", "--test-changed-package-only"]).is_ok());
        assert!(Cli::try_parse_from([
            "slop-gate",
            "--test-changed-package-only",
            "--test-workspace"
        ])
        .is_err());
    }

    #[test]
    fn advisory_turns_the_mcp_lane_advisory_too() {
        // `--advisory` is how a team adopts the gate without it blocking. If it
        // silenced the mutation gate but left the MCP lane blocking, the flag
        // would mean something different on exactly the repos this lane targets.
        let args = RunArgs {
            advisory: true,
            ..base_args()
        };
        assert_eq!(args.to_config().unwrap().mcp_fail_on, "never");
    }

    #[test]
    fn an_explicit_mcp_threshold_outranks_advisory() {
        // Narrower instruction wins: "--advisory --mcp-fail-on critical" is a
        // coherent thing to ask for (score the mutations, gate the server).
        let args = RunArgs {
            advisory: true,
            mcp_fail_on: Some("critical".into()),
            ..base_args()
        };
        assert_eq!(args.to_config().unwrap().mcp_fail_on, "critical");
    }

    #[test]
    fn a_misspelled_mcp_threshold_is_rejected_before_anything_runs() {
        let args = RunArgs {
            mcp_fail_on: Some("crticial".into()),
            ..base_args()
        };
        let e = args.to_config().unwrap_err().to_string();
        assert!(e.contains("crticial"), "{e}");
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
