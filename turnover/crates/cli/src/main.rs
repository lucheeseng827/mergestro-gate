//! `turnover` — longitudinal AI-code maintainability gate.
//!
//!   turnover baseline            walk history once (then incrementally) into .turnover/baseline.json
//!   turnover gate                evaluate the trailing window against the baseline; exit 1 on fail
//!   turnover gate --base-ref X   evaluate only the commits this branch adds over X
//!   turnover report              the trend series the baseline already contains
//!   turnover explain <rev>       per-file, per-line classification of one commit
//!
//! Argument parsing only: every decision lives in `turnover-gate` and below.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use turnover_core::window::series;
use turnover_gate::config::Config;
use turnover_gate::record;
use turnover_gate::render;
use turnover_gate::run::{self, ymd, BaselineRequest, GateError, GateRequest, Scope};
use turnover_history::Stats;

#[derive(Parser)]
#[command(name = "turnover", version, about = "Longitudinal AI-code maintainability gate", long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Walk the repository's history into a baseline file (incremental after the first run).
    Baseline(BaselineArgs),
    /// Evaluate the gate against the baseline and exit non-zero on failure (blocking mode).
    Gate(GateArgs),
    /// Print the trend series held in the baseline.
    Report(ReportArgs),
    /// Explain how one commit was classified, file by file.
    Explain(ExplainArgs),
}

#[derive(Args, Clone)]
struct Common {
    /// Repository path (a worktree or any directory inside one).
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Config file (default: ./turnover.toml if present, else built-in defaults).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Baseline file.
    #[arg(long, default_value = ".turnover/baseline.json")]
    baseline: PathBuf,
    /// Worker threads (default: all cores).
    #[arg(long)]
    jobs: Option<usize>,
}

#[derive(Args)]
struct BaselineArgs {
    #[command(flatten)]
    common: Common,
    /// Revision to walk from.
    #[arg(long, default_value = "HEAD")]
    tip: String,
    /// Only walk commits from the last N days (first run only; refreshes walk from the old head).
    #[arg(long)]
    since_days: Option<u32>,
    /// Discard the existing baseline and walk from scratch.
    #[arg(long)]
    full: bool,
    /// After building, upload the baseline to this Mergestro plane (baseline service).
    #[arg(long)]
    push_url: Option<String>,
    /// Bearer token for the plane (default: $TURNOVER_TOKEN, else $METRICS_TOKEN).
    #[arg(long)]
    token: Option<String>,
    /// Repository name on the plane (default: $GITHUB_REPOSITORY, else the directory name).
    #[arg(long)]
    repo_name: Option<String>,
}

#[derive(Args)]
struct GateArgs {
    #[command(flatten)]
    common: Common,
    /// Evaluate the commits reachable from HEAD but not from this ref (a PR), instead of a time window.
    #[arg(long)]
    base_ref: Option<String>,
    /// Override the policy's trailing window.
    #[arg(long)]
    window_days: Option<u32>,
    /// Report only; never exit non-zero on a failed check.
    #[arg(long)]
    advisory: bool,
    /// Do not walk new commits before evaluating (use the baseline exactly as stored).
    #[arg(long)]
    no_refresh: bool,
    /// Write the refreshed baseline back to disk.
    #[arg(long)]
    update_baseline: bool,
    /// Fetch the baseline from this Mergestro plane before judging (and push it back after,
    /// with --update-baseline). A plane with no copy yet falls through to the local file.
    #[arg(long)]
    baseline_url: Option<String>,
    /// Bearer token for the plane (default: $TURNOVER_TOKEN, else $METRICS_TOKEN).
    #[arg(long)]
    token: Option<String>,
    /// Write the full JSON report here.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Print the Markdown section a PR comment would carry instead of the text report.
    #[arg(long)]
    markdown: bool,
    /// Append a Mergestro `turnover` record (JSON-Lines) here.
    #[arg(long)]
    emit: Option<PathBuf>,
    /// Repository name for the emitted record (default: $GITHUB_REPOSITORY, else the directory name).
    #[arg(long)]
    repo_name: Option<String>,
    /// PR number for the emitted record (default: parsed from $GITHUB_REF_NAME).
    #[arg(long)]
    pr: Option<u64>,
    /// Run id for the emitted record (default: $GITHUB_RUN_ID, else local-<unix time>).
    #[arg(long)]
    run_id: Option<String>,
    /// Workflow actor for the emitted record (default: $GITHUB_ACTOR).
    #[arg(long)]
    actor: Option<String>,
    /// PR author for the emitted record (default: $TURNOVER_PR_AUTHOR).
    #[arg(long)]
    pr_author: Option<String>,
}

#[derive(Args)]
struct ReportArgs {
    #[command(flatten)]
    common: Common,
    /// Bucket width for the series.
    #[arg(long, default_value_t = 30)]
    bucket_days: u32,
    /// Print the series as JSON instead of a table.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ExplainArgs {
    #[command(flatten)]
    common: Common,
    /// The commit to explain.
    rev: String,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
}

fn main() {
    let cli = Cli::parse();
    let code = match run_cli(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("turnover: error: {e:#}");
            2
        }
    };
    std::process::exit(code);
}

fn run_cli(cli: Cli) -> anyhow::Result<i32> {
    match cli.cmd {
        Cmd::Baseline(a) => cmd_baseline(a).map(|_| 0),
        Cmd::Gate(a) => cmd_gate(a),
        Cmd::Report(a) => cmd_report(a).map(|_| 0),
        Cmd::Explain(a) => cmd_explain(a).map(|_| 0),
    }
}

fn setup(common: &Common) -> anyhow::Result<Config> {
    if let Some(n) = common.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
            .ok();
    }
    Config::load(common.config.as_deref())
}

fn cmd_baseline(a: BaselineArgs) -> anyhow::Result<()> {
    let config = setup(&a.common)?;
    let summary = run::build_baseline(&BaselineRequest {
        repo: a.common.repo.clone(),
        config,
        baseline_path: a.common.baseline.clone(),
        tip: a.tip.clone(),
        since_days: a.since_days,
        full: a.full,
    })?;
    eprintln!(
        "turnover: baseline {} — {} new commits classified, {} total ({:.1}s)",
        summary.path.display(),
        summary.new_commits,
        summary.total_commits,
        summary.elapsed_secs
    );
    if let Some(s) = render::stats_line(&summary.walk) {
        eprintln!("turnover: {s}");
    }
    if let Some(all) = &summary.whole_history {
        eprintln!(
            "turnover: whole-history ratios — {}",
            render::ratio_line(all)
        );
    }
    if let Some(url) = &a.push_url {
        let token = a
            .token
            .clone()
            .or_else(turnover_gate::remote::token_from_env)
            .unwrap_or_default();
        let repo = a
            .repo_name
            .clone()
            .unwrap_or_else(|| run::repo_display_name(&a.common.repo));
        turnover_gate::remote::push(url, &token, &repo, &a.common.baseline)?;
        eprintln!("turnover: baseline pushed to {url} for {repo}");
    }
    Ok(())
}

#[derive(Serialize)]
struct GateReport<'a> {
    repo: String,
    #[serde(flatten)]
    outcome: &'a run::GateOutcome,
}

fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.is_empty())
}

fn pr_from_env() -> Option<u64> {
    let r = env_nonempty("GITHUB_REF_NAME")?;
    r.strip_suffix("/merge").and_then(|n| n.parse().ok())
}

fn repo_display_name(common: &Common) -> String {
    env_nonempty("GITHUB_REPOSITORY").unwrap_or_else(|| {
        std::fs::canonicalize(&common.repo)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "repo".to_string())
    })
}

fn cmd_gate(a: GateArgs) -> anyhow::Result<i32> {
    let config = setup(&a.common)?;
    let plane_repo = a
        .repo_name
        .clone()
        .unwrap_or_else(|| repo_display_name(&a.common));
    let token = a
        .token
        .clone()
        .or_else(turnover_gate::remote::token_from_env)
        .unwrap_or_default();
    if let Some(url) = &a.baseline_url {
        if turnover_gate::remote::fetch(url, &token, &plane_repo, &a.common.baseline)? {
            eprintln!("turnover: baseline fetched from {url} for {plane_repo}");
        } else {
            eprintln!(
                "turnover: {url} has no baseline for {plane_repo} yet; using the local file if any"
            );
        }
    }
    let req = GateRequest {
        repo: a.common.repo.clone(),
        config,
        baseline_path: a.common.baseline.clone(),
        scope: match &a.base_ref {
            Some(b) => Scope::BaseRef(b.clone()),
            None => Scope::Trailing {
                window_days: a.window_days,
            },
        },
        refresh: !a.no_refresh,
        update_baseline: a.update_baseline,
        advisory: a.advisory,
    };
    let outcome = match run::run_gate(&req) {
        Ok(o) => o,
        Err(e @ GateError::MissingBaseline(_)) | Err(e @ GateError::ConfigMismatch) => bail!("{e}"),
        Err(GateError::Other(e)) => return Err(e),
    };
    if a.markdown {
        print!("{}", render::markdown(&outcome));
    } else {
        print!("{}", render::text(&outcome));
    }
    if let (Some(url), true) = (&a.baseline_url, a.update_baseline) {
        turnover_gate::remote::push(url, &token, &plane_repo, &a.common.baseline)?;
        eprintln!("turnover: refreshed baseline pushed to {url} for {plane_repo}");
    }

    let repo_name = a
        .repo_name
        .clone()
        .unwrap_or_else(|| repo_display_name(&a.common));
    if let Some(path) = &a.json {
        let report = GateReport {
            repo: repo_name.clone(),
            outcome: &outcome,
        };
        std::fs::write(path, serde_json::to_string_pretty(&report)?)
            .with_context(|| format!("write {}", path.display()))?;
        eprintln!("turnover: report written to {}", path.display());
    }
    if let Some(path) = &a.emit {
        let ts = run::now_unix().max(0) as u64;
        let id = record::Identity {
            repo: repo_name,
            pr: a.pr.or_else(pr_from_env),
            head_sha: outcome.head_sha.clone(),
            run_id: a
                .run_id
                .clone()
                .or_else(|| env_nonempty("GITHUB_RUN_ID"))
                .unwrap_or_else(|| format!("local-{ts}")),
            actor: a.actor.clone().or_else(|| env_nonempty("GITHUB_ACTOR")),
            pr_author: a
                .pr_author
                .clone()
                .or_else(|| env_nonempty("TURNOVER_PR_AUTHOR")),
            timestamp_unix: ts,
        };
        let rec = record::build(
            id,
            outcome.window_days,
            &outcome.verdict,
            &outcome.window,
            &outcome.baseline,
        );
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        writeln!(f, "{}", serde_json::to_string(&rec)?)?;
        eprintln!("turnover: record appended to {}", path.display());
    }
    Ok(outcome.verdict.exit_code())
}

fn cmd_report(a: ReportArgs) -> anyhow::Result<()> {
    let Some(baseline) = run::read_baseline(&a.common.baseline)? else {
        bail!(
            "no baseline at {} — run `turnover baseline` first",
            a.common.baseline.display()
        );
    };
    let (lo, hi) = run::span(&baseline);
    let s = series(&baseline.commits, lo, hi, i64::from(a.bucket_days) * 86_400);
    if a.json {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(());
    }
    println!(
        "turnover: {} — {} commits, {} → {}, {}-day buckets",
        baseline.repo.as_deref().unwrap_or("repository"),
        baseline.commits.len(),
        ymd(lo),
        ymd(hi - 1),
        a.bucket_days
    );
    println!(
        "  {:<10} {:>7} {:>8} {:>10} {:>9} {:>8} {:>7}",
        "from", "commits", "added", "copy/paste", "dup block", "refactor", "churn"
    );
    for b in &s {
        println!(
            "  {:<10} {:>7} {:>8} {:>10} {:>9} {:>8} {:>7}",
            ymd(b.from_unix),
            b.commits,
            b.counts.added,
            render::pct(b.ratios.copy_paste),
            render::pct(b.ratios.dup_block),
            render::pct(b.ratios.refactor),
            render::pct(b.ratios.churn)
        );
    }
    Ok(())
}

fn cmd_explain(a: ExplainArgs) -> anyhow::Result<()> {
    let cfg = setup(&a.common)?;
    let repo = turnover_history::open(&a.common.repo.to_string_lossy())?;
    let opts = cfg.walk.to_options();
    let (row, files, stats) = turnover_history::explain_commit(&repo, &a.rev, &opts, &cfg.signals)?;
    if a.json {
        #[derive(Serialize)]
        struct Out<'a> {
            commit: &'a turnover_core::CommitSignals,
            files: &'a [turnover_core::signals::FileExplanation],
            walk: &'a Stats,
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&Out {
                commit: &row,
                files: &files,
                walk: &stats
            })?
        );
        return Ok(());
    }
    println!(
        "commit {} ({}) by {} — {} files: added {} · deleted {} · moved {} · copy/pasted {} · dup block {}",
        row.sha,
        ymd(row.timestamp_unix),
        row.author,
        row.files,
        row.counts.added,
        row.counts.deleted,
        row.counts.moved,
        row.counts.copy_pasted,
        row.counts.dup_block
    );
    for f in &files {
        println!(
            "  {} [{}] +{} -{} moved {} pasted {} block {}",
            f.path,
            f.language,
            f.counts.added,
            f.counts.deleted,
            f.counts.moved,
            f.counts.copy_pasted,
            f.counts.dup_block
        );
        if !f.moved_lines.is_empty() {
            println!("      moved lines:   {}", join(&f.moved_lines));
        }
        if !f.copy_pasted_lines.is_empty() {
            println!("      pasted lines:  {}", join(&f.copy_pasted_lines));
        }
        if !f.dup_block_lines.is_empty() {
            println!("      dup block:     {}", join(&f.dup_block_lines));
        }
    }
    if let Some(s) = render::stats_line(&stats) {
        eprintln!("turnover: {s}");
    }
    Ok(())
}

fn join(v: &[usize]) -> String {
    v.iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",")
}
