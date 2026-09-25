//! Building the baseline and running the gate. The two front-ends differ only in how they
//! fill a [`GateRequest`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, Context};
use serde::Serialize;
use turnover_core::policy::{evaluate, Mode, Policy, Verdict};
use turnover_core::signals::DupBlock;
use turnover_core::window::{aggregate, Aggregate};
use turnover_core::Baseline;
use turnover_history::{CommitMeta, Stats, WalkOptions};

use crate::config::Config;

/// Which commits the gate judges.
#[derive(Debug, Clone)]
pub enum Scope {
    /// The trailing window from the policy (or an override), ending at the newest commit.
    Trailing { window_days: Option<u32> },
    /// The commits reachable from HEAD but not from this ref — a pull request.
    BaseRef(String),
}

#[derive(Debug, Clone)]
pub struct GateRequest {
    pub repo: PathBuf,
    pub config: Config,
    pub baseline_path: PathBuf,
    pub scope: Scope,
    /// Walk commits newer than the stored baseline head before evaluating.
    pub refresh: bool,
    /// Persist the refreshed baseline.
    pub update_baseline: bool,
    /// Force advisory mode regardless of the policy.
    pub advisory: bool,
}

impl GateRequest {
    pub fn new(repo: impl Into<PathBuf>, config: Config) -> Self {
        GateRequest {
            repo: repo.into(),
            config,
            baseline_path: PathBuf::from(".turnover/baseline.json"),
            scope: Scope::Trailing { window_days: None },
            refresh: true,
            update_baseline: false,
            advisory: false,
        }
    }
}

/// What the gate concluded, plus everything a report needs.
#[derive(Debug, Clone, Serialize)]
pub struct GateOutcome {
    pub tool_version: &'static str,
    pub head_sha: String,
    pub generated_at_unix: i64,
    /// Human description of the window ("trailing 90 days (…)", "commits in HEAD not in X").
    pub scope: String,
    pub window_days: u32,
    pub verdict: Verdict,
    pub window: Aggregate,
    pub baseline: Aggregate,
    /// `false` when no rows fall outside the window yet — only absolute caps applied.
    pub has_baseline: bool,
    pub baseline_commits: usize,
    pub walk: Stats,
    /// The files that carry the window's duplication, worst first — the evidence behind the
    /// verdict, with each duplicated block's twin. Empty when explaining was capped out.
    pub offenders: Vec<FileOffender>,
    /// Commits explained for `offenders` (≤ the configured cap).
    pub explained_commits: usize,
    #[serde(skip)]
    pub policy: Policy,
}

/// One file's contribution to the window's copy/paste and duplication, in one commit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileOffender {
    pub path: String,
    pub language: String,
    pub sha: String,
    pub added: u64,
    pub copy_pasted: u64,
    pub dup_block: u64,
    pub moved: u64,
    /// First pasted lines (1-based, in that commit's version of the file).
    pub pasted_lines: Vec<usize>,
    pub dup_blocks: Vec<DupBlock>,
}

impl FileOffender {
    /// Lines of duplication this file added: the sort key.
    pub fn weight(&self) -> u64 {
        self.copy_pasted + self.dup_block
    }
}

/// Explain the window's commits file by file and keep the heaviest files.
fn offenders(
    repo: &gix::ThreadSafeRepository,
    metas: &[CommitMeta],
    opts: &WalkOptions,
    cfg: &Config,
) -> anyhow::Result<(Vec<FileOffender>, usize)> {
    if cfg.report.explain_commits == 0 || cfg.report.offenders == 0 || metas.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let take: Vec<CommitMeta> = metas
        .iter()
        .take(cfg.report.explain_commits)
        .cloned()
        .collect();
    let explained = turnover_history::explain_commits(repo, &take, opts, &cfg.signals)?;
    let mut out: Vec<FileOffender> = Vec::new();
    for (row, files) in &explained {
        for f in files {
            if f.counts.copy_pasted == 0 && f.counts.dup_block == 0 {
                continue;
            }
            out.push(FileOffender {
                path: f.path.clone(),
                language: f.language.clone(),
                sha: row.sha.chars().take(10).collect(),
                added: f.counts.added,
                copy_pasted: f.counts.copy_pasted,
                dup_block: f.counts.dup_block,
                moved: f.counts.moved,
                pasted_lines: f.copy_pasted_lines.iter().take(12).copied().collect(),
                dup_blocks: f.dup_blocks.iter().take(4).cloned().collect(),
            });
        }
    }
    out.sort_by(|a, b| {
        b.weight()
            .cmp(&a.weight())
            .then_with(|| a.path.cmp(&b.path))
    });
    out.truncate(cfg.report.offenders);
    Ok((out, take.len()))
}

#[derive(Debug, thiserror::Error)]
pub enum GateError {
    /// The gate has nothing to compare against until a baseline exists.
    #[error("no baseline at {0} — run `turnover baseline` first (the gate has nothing to compare against until it exists)")]
    MissingBaseline(PathBuf),
    #[error("classifier settings differ from the baseline's; run `turnover baseline --full` to rebuild it")]
    ConfigMismatch,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn read_baseline(path: &Path) -> anyhow::Result<Option<Baseline>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(Some(
        Baseline::from_json(&text).with_context(|| format!("parse {}", path.display()))?,
    ))
}

pub fn write_baseline(path: &Path, b: &Baseline) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, b.to_json()?).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

fn progress(n: usize) {
    if n % 200 == 0 {
        eprint!("\rturnover: classified {n} commits");
    }
}

/// Walk the commits `opts` selects and fold them into `baseline`. Returns (new rows, stats).
fn refresh(
    repo: &gix::ThreadSafeRepository,
    cfg: &Config,
    baseline: &mut Baseline,
    opts: &WalkOptions,
) -> anyhow::Result<(usize, Stats)> {
    let metas = turnover_history::list_commits(repo, opts)?;
    let known: HashSet<String> = baseline
        .known_shas()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let metas: Vec<_> = metas
        .into_iter()
        .filter(|m| !known.contains(&m.id.to_string()))
        .collect();
    if metas.is_empty() {
        return Ok((0, Stats::default()));
    }
    let (rows, stats) =
        turnover_history::classify_all(repo, &metas, opts, &cfg.signals, &progress)?;
    if stats.commits >= 200 {
        eprintln!();
    }
    let new = rows.len();
    let mut existing = std::mem::take(&mut baseline.commits);
    let pending = baseline.pending_additions();
    let still_open =
        turnover_history::attribute_churn(&mut existing, rows, &pending, cfg.churn.horizon_secs());
    baseline.commits = existing;
    baseline.commits.sort_by(|a, b| {
        a.timestamp_unix
            .cmp(&b.timestamp_unix)
            .then_with(|| a.sha.cmp(&b.sha))
    });
    baseline.set_pending_additions(still_open);
    baseline.churn_horizon_secs = cfg.churn.horizon_secs();
    Ok((new, stats))
}

/// The repository's name for records and the baseline service: `$GITHUB_REPOSITORY`, else
/// the directory name.
pub fn repo_display_name(repo: &Path) -> String {
    std::env::var("GITHUB_REPOSITORY")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::fs::canonicalize(repo)
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "repo".to_string())
        })
}

#[derive(Debug, Clone)]
pub struct BaselineRequest {
    pub repo: PathBuf,
    pub config: Config,
    pub baseline_path: PathBuf,
    pub tip: String,
    /// First run only: ignore commits older than this many days.
    pub since_days: Option<u32>,
    /// Discard the existing file and walk from scratch.
    pub full: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BaselineSummary {
    pub path: PathBuf,
    pub new_commits: usize,
    pub total_commits: usize,
    pub elapsed_secs: f64,
    pub walk: Stats,
    pub rebuilt: bool,
    pub whole_history: Option<Aggregate>,
}

/// Build or refresh the baseline file.
pub fn build_baseline(req: &BaselineRequest) -> anyhow::Result<BaselineSummary> {
    let started = Instant::now();
    let cfg = &req.config;
    let repo = turnover_history::open(&req.repo.to_string_lossy())?;
    let mut rebuilt = false;
    let mut baseline = match (req.full, read_baseline(&req.baseline_path)?) {
        (false, Some(b)) if !b.config_differs(&cfg.signals) => b,
        (false, Some(_)) => {
            eprintln!("turnover: classifier settings changed since the baseline was built; rebuilding from scratch");
            rebuilt = true;
            Baseline::new(cfg.signals.clone(), now_unix())
        }
        _ => {
            rebuilt = true;
            Baseline::new(cfg.signals.clone(), now_unix())
        }
    };
    let mut opts = cfg.walk_options();
    opts.tips = vec![req.tip.clone()];
    if let Some(head) = baseline.head.clone() {
        opts.hidden = vec![head];
        opts.lenient_hidden = true;
    } else if let Some(days) = req.since_days {
        opts.since_unix = Some(now_unix() - i64::from(days) * 86_400);
    }
    let (new_commits, walk) = refresh(&repo, cfg, &mut baseline, &opts)?;
    baseline.head = Some(turnover_history::resolve_sha(&repo, &req.tip)?);
    baseline.generated_at_unix = now_unix();
    if baseline.repo.is_none() {
        baseline.repo = Some(repo_display_name(&req.repo));
    }
    write_baseline(&req.baseline_path, &baseline)?;
    let whole_history = match (baseline.oldest_timestamp(), baseline.newest_timestamp()) {
        (Some(o), Some(n)) => Some(aggregate(&baseline.commits, o, n + 1)),
        _ => None,
    };
    Ok(BaselineSummary {
        path: req.baseline_path.clone(),
        new_commits,
        total_commits: baseline.commits.len(),
        elapsed_secs: started.elapsed().as_secs_f64(),
        walk,
        rebuilt,
        whole_history,
    })
}

/// `(oldest, newest + 1)` over the baseline rows — the half-open range holding everything.
pub fn span(b: &Baseline) -> (i64, i64) {
    let lo = b.oldest_timestamp().unwrap_or(0);
    let hi = b.newest_timestamp().map(|t| t + 1).unwrap_or(1);
    (lo, hi)
}

/// Evaluate the gate. Refreshes the baseline in memory first unless `refresh` is off.
pub fn run_gate(req: &GateRequest) -> Result<GateOutcome, GateError> {
    let mut cfg = req.config.clone();
    if let Scope::Trailing {
        window_days: Some(d),
    } = req.scope
    {
        cfg.policy.gate.window_days = d;
    }
    if req.advisory {
        cfg.policy.gate.mode = Mode::Advisory;
    }
    let Some(mut baseline) = read_baseline(&req.baseline_path)? else {
        return Err(GateError::MissingBaseline(req.baseline_path.clone()));
    };
    if baseline.config_differs(&cfg.signals) {
        return Err(GateError::ConfigMismatch);
    }
    let repo = turnover_history::open(&req.repo.to_string_lossy()).map_err(anyhow::Error::from)?;
    let mut walk = Stats::default();
    if req.refresh {
        let mut opts = cfg.walk_options();
        if let Some(head) = baseline.head.clone() {
            opts.hidden = vec![head];
            opts.lenient_hidden = true;
        }
        let (_, s) = refresh(&repo, &cfg, &mut baseline, &opts)?;
        walk = s;
        baseline.head =
            Some(turnover_history::resolve_sha(&repo, "HEAD").map_err(anyhow::Error::from)?);
        baseline.generated_at_unix = now_unix();
        if req.update_baseline {
            write_baseline(&req.baseline_path, &baseline)?;
        }
    }
    let head_sha = turnover_history::resolve_sha(&repo, "HEAD").map_err(anyhow::Error::from)?;

    let walk_opts = cfg.walk_options();
    let (scope, window, base_agg, window_metas) = match &req.scope {
        Scope::BaseRef(base) => {
            let mut o = cfg.walk_options();
            o.hidden = vec![base.clone()];
            let metas = turnover_history::list_commits(&repo, &o).map_err(anyhow::Error::from)?;
            let branch: HashSet<String> = metas.iter().map(|m| m.id.to_string()).collect();
            if branch.is_empty() {
                return Err(anyhow!("no commits in HEAD that are not in {base}").into());
            }
            let (lo, hi) = span(&baseline);
            let w = aggregate(
                baseline.commits.iter().filter(|r| branch.contains(&r.sha)),
                lo,
                hi,
            );
            let b = aggregate(
                baseline.commits.iter().filter(|r| !branch.contains(&r.sha)),
                lo,
                hi,
            );
            (
                format!("commits in HEAD not in {base} ({} commits)", branch.len()),
                w,
                b,
                metas,
            )
        }
        Scope::Trailing { .. } => {
            let (lo, hi) = span(&baseline);
            let from = hi - i64::from(cfg.policy.gate.window_days) * 86_400;
            let w = aggregate(&baseline.commits, from, hi);
            let b = aggregate(&baseline.commits, lo, from);
            // The window's commits, newest first, for the offenders list.
            let mut o = cfg.walk_options();
            o.since_unix = Some(from);
            let metas = turnover_history::list_commits(&repo, &o).map_err(anyhow::Error::from)?;
            (
                format!(
                    "trailing {} days ({} → {})",
                    cfg.policy.gate.window_days,
                    ymd(from),
                    ymd(hi - 1)
                ),
                w,
                b,
                metas,
            )
        }
    };
    let (offenders, explained_commits) = offenders(&repo, &window_metas, &walk_opts, &cfg)?;
    let has_baseline = base_agg.counts.added > 0;
    let verdict = evaluate(&cfg.policy, has_baseline.then_some(&base_agg), &window);
    Ok(GateOutcome {
        tool_version: env!("CARGO_PKG_VERSION"),
        head_sha,
        generated_at_unix: now_unix(),
        scope,
        window_days: cfg.policy.gate.window_days,
        verdict,
        window,
        baseline: base_agg,
        has_baseline,
        baseline_commits: baseline.commits.len(),
        walk,
        offenders,
        explained_commits,
        policy: cfg.policy,
    })
}

/// `YYYY-MM-DD` (UTC) from unix seconds; no chrono for one formatter.
pub fn ymd(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ymd_matches_known_dates() {
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(951_782_400), "2000-02-29");
        assert_eq!(ymd(1_788_220_800), "2026-09-01");
    }

    #[test]
    fn a_missing_baseline_is_a_typed_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut req = GateRequest::new(tmp.path(), Config::default());
        req.baseline_path = tmp.path().join("none.json");
        match run_gate(&req) {
            Err(GateError::MissingBaseline(p)) => assert_eq!(p, tmp.path().join("none.json")),
            other => panic!("expected MissingBaseline, got {other:?}"),
        }
    }
}
