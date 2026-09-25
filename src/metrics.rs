// SPDX-License-Identifier: Apache-2.0
//! Phase 3 — validation telemetry.
//!
//! Phase 3 is fundamentally a *data* phase: prove teams keep the gate on, that
//! survivors get acted on, and pin the ICP from real usage. None of that can be
//! decided in code — but it can only be decided if the gate **emits** the right
//! signal. This module turns each run into a compact, append-only record
//! ([`RunMetrics`]) carrying exactly the keys the Phase 3 questions need:
//!
//! - survivor counts and per-survivor fingerprints (→ fix-vs-override, false
//!   positives, survivor trend),
//! - the run mode, blocking vs advisory, and the verdict (→ block rate,
//!   blocking-vs-downgraded),
//! - duration (→ latency tolerance),
//! - repo / PR / commit identity (→ retention, disable rate, per-PR grouping).
//!
//! Records are written as JSON Lines (one object per run) so they append
//! cheaply across CI runs and stream into any analysis. [`crate::analyze`]
//! reads them back into the headline KPIs.

use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::report::{GateReport, Mutant, Verdict};
use crate::severity::SeverityCounts;

/// Schema version for forward-compatible ingestion. Bumped to 2 in Phase 4 with
/// `mutation_score`, `severity_counts`, `debt_score`; to 3 with the slop lane
/// (`slop_score`, `slop_findings`); to 4 with the security lane
/// (`security_score`, `security_findings`). All additions default on read, so
/// older records still parse; to 5 with the convention lane
/// (`convention_score`, `convention_findings`); to 6 with the `record_type`
/// ingest discriminator (always `"slop"`), so an emitted line drops straight
/// into Mergestro's `IngestRecord::Slop` envelope; to 7 with `pr_author`, the
/// PR's author as distinct from `actor` (the workflow triggerer) — the identity
/// a per-active-developer rollup needs and `actor` cannot supply.
pub const SCHEMA_VERSION: u32 = 7;

/// The `record_type` tag every gate record carries. A free fn so it can name the
/// `#[serde(default = ...)]` for records written before v6 (which had no tag).
fn default_record_type() -> String {
    "slop".to_string()
}

/// One run's worth of validation telemetry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunMetrics {
    /// Discriminator for Mergestro's ingest envelope ([`records.rs::IngestRecord`] on the control
    /// plane), always `"slop"` — mirrors the `"queue"` tag Mergestro Queue's `QueueRunMetrics` carries.
    /// It makes a line emitted here parse directly into `IngestRecord::Slop` at `/v1/ingest`; the
    /// EE ignores the extra gate-only fields below. `#[serde(default)]` keeps pre-v6 local records
    /// (written without the tag) parsing when [`crate::analyze`] reads them back.
    #[serde(default = "default_record_type")]
    pub record_type: String,
    pub schema_version: u32,
    pub gate_version: String,
    /// Seconds since the Unix epoch.
    pub timestamp_unix: u64,

    // Identity — lets the analyser group by PR and order runs over time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// The PR's **author**, which `actor` is not: `actor` is `GITHUB_ACTOR`, the workflow
    /// triggerer, and on a merge-queue re-run that is the bot. Sourced from `PR_AUTHOR`
    /// (the Action fills it from the pull-request payload). Optional everywhere, so a
    /// local run or an un-wired CI simply omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_author: Option<String>,

    /// `"blocking"` (the gate can fail the build) or `"advisory"`.
    pub mode: String,
    /// `"pass"` or `"block"`.
    pub verdict: String,
    pub block_reasons: Vec<String>,

    pub changed_rust_files: usize,
    pub candidates: usize,
    pub capped_out: usize,
    pub tested: usize,
    pub caught: usize,
    pub survivors: usize,
    pub timed_out: usize,
    pub unviable: usize,
    /// Stable-ish identity per survivor, so the same survivor can be tracked
    /// across an evolving PR (fix-vs-override). See [`survivor_fingerprint`].
    pub survivor_fingerprints: Vec<String>,
    pub zero_assertion_tests: usize,

    // ── Phase 4: rollout signals ────────────────────────────────────────────
    /// `caught / tested` for this run — the headline trend metric. `None` when
    /// nothing was tested (no Rust changed, or the suite wasn't trustworthy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutation_score: Option<f64>,
    /// Survivor severity histogram, so the trend can show the risk mix.
    #[serde(default)]
    pub severity_counts: SeverityCounts,
    /// Net structural debt the diff added (`None` when nothing changed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debt_score: Option<i64>,

    // ── Pattern lane: AI-slop signatures (Track B) ──────────────────────────
    /// 0–100 slop-likeness for this run (`None` when nothing was flagged) — the
    /// signal the Track C "is slop score believable?" question trends on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slop_score: Option<u32>,
    /// Count of slop signatures found on the changed surface.
    #[serde(default)]
    pub slop_findings: usize,
    /// 0–100 security anti-pattern score (`None` when nothing was flagged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_score: Option<u32>,
    /// Count of security anti-patterns found on the changed surface.
    #[serde(default)]
    pub security_findings: usize,
    /// 0–100 convention/hallucinated-import score (`None` when nothing flagged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub convention_score: Option<u32>,
    /// Count of unknown-crate imports found on the changed surface.
    #[serde(default)]
    pub convention_findings: usize,

    // ── Turnover lane (turnover) ───────────────────────────────────────────
    /// The change's maintainability drift, when the lane ran. The full `turnover`
    /// record travels as its own JSON line beside this one (see
    /// [`turnover_record_line`]); this is the summary the slop trend reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turnover: Option<TurnoverMetrics>,

    pub duration_secs: f64,
}

impl RunMetrics {
    /// Build a record from a finished report and the run's config + context.
    pub fn from_report(report: &GateReport, cfg: &Config, ctx: &RunContext) -> Self {
        let (verdict, block_reasons) = match &report.verdict {
            Verdict::Pass => ("pass".to_string(), Vec::new()),
            Verdict::Block { reasons } => ("block".to_string(), reasons.clone()),
        };
        RunMetrics {
            record_type: default_record_type(),
            schema_version: SCHEMA_VERSION,
            gate_version: env!("CARGO_PKG_VERSION").to_string(),
            timestamp_unix: ctx.timestamp_unix,
            repo: ctx.repo.clone(),
            pr: ctx.pr,
            head_sha: ctx.head_sha.clone(),
            run_id: ctx.run_id.clone(),
            actor: ctx.actor.clone(),
            pr_author: ctx.pr_author.clone(),
            mode: if cfg.block_on_survivors {
                "blocking".to_string()
            } else {
                "advisory".to_string()
            },
            verdict,
            block_reasons,
            changed_rust_files: report.changed_rust_files.len(),
            candidates: report.candidates,
            capped_out: report.capped_out,
            tested: report.tested,
            caught: report.caught,
            survivors: report.survivors.len(),
            timed_out: report.timed_out,
            unviable: report.unviable,
            survivor_fingerprints: report.survivors.iter().map(survivor_fingerprint).collect(),
            zero_assertion_tests: report.zero_assertion_tests.len(),
            mutation_score: if report.tested > 0 {
                Some(report.caught as f64 / report.tested as f64)
            } else {
                None
            },
            severity_counts: SeverityCounts::of(&report.survivors),
            debt_score: report.debt.as_ref().map(|d| d.score()),
            slop_score: report.slop.as_ref().map(|s| s.score),
            slop_findings: report.slop.as_ref().map_or(0, |s| s.findings.len()),
            security_score: report.security.as_ref().map(|s| s.score),
            security_findings: report.security.as_ref().map_or(0, |s| s.findings.len()),
            convention_score: report.convention.as_ref().map(|s| s.score),
            convention_findings: report.convention.as_ref().map_or(0, |s| s.findings.len()),
            duration_secs: report.duration_secs,
            turnover: report.turnover.as_ref().map(|t| TurnoverMetrics {
                verdict: t.status.clone(),
                mode: t.mode.clone(),
                window_added: t.window.counts.added,
                window_commits: t.window.commits,
                ratios: t.window.ratios,
                baseline_ratios: t.baseline.ratios,
                failed_checks: t.failed_checks.clone(),
            }),
        }
    }

    /// Serialize as a single JSON line (no trailing newline).
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self)
            // Serialising a plain struct of standard types effectively never
            // fails; if it somehow did, still emit *valid* JSON (let serde_json
            // escape the error message rather than interpolating it raw).
            .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }).to_string())
    }
}

/// Identity/timing context, assembled from the environment (best-effort).
#[derive(Debug, Clone, Default)]
pub struct RunContext {
    pub timestamp_unix: u64,
    pub repo: Option<String>,
    pub pr: Option<u64>,
    pub head_sha: Option<String>,
    pub run_id: Option<String>,
    pub actor: Option<String>,
    pub pr_author: Option<String>,
}

impl RunContext {
    /// Read context from the standard GitHub Actions environment, falling back
    /// to `None` for anything absent so this works locally too.
    pub fn from_env() -> Self {
        RunContext {
            timestamp_unix: now_unix(),
            repo: env_nonempty("GITHUB_REPOSITORY"),
            pr: detect_pr_number(),
            head_sha: env_nonempty("PR_HEAD_SHA").or_else(|| env_nonempty("GITHUB_SHA")),
            run_id: env_nonempty("GITHUB_RUN_ID"),
            actor: env_nonempty("GITHUB_ACTOR"),
            pr_author: env_nonempty("PR_AUTHOR"),
        }
    }
}

/// The turnover lane's own ingest record (`record_type: "turnover"`), as one JSON line, when
/// the lane measured something. Skipped lanes emit nothing: a run that decided nothing must
/// not become a point on a trend.
pub fn turnover_record_line(report: &GateReport, ctx: &RunContext) -> Option<String> {
    let t = report.turnover.as_ref()?;
    let verdict = t.verdict.as_ref()?;
    let head_sha = ctx
        .head_sha
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| t.head_sha.clone());
    let identity = turnover_gate::record::Identity {
        repo: ctx.repo.clone().unwrap_or_else(|| "unknown".to_string()),
        pr: ctx.pr,
        head_sha,
        run_id: ctx
            .run_id
            .clone()
            .unwrap_or_else(|| format!("local-{}", ctx.timestamp_unix)),
        actor: ctx.actor.clone(),
        pr_author: ctx.pr_author.clone(),
        timestamp_unix: ctx.timestamp_unix,
    };
    let rec =
        turnover_gate::record::build(identity, t.window_days, verdict, &t.window, &t.baseline);
    serde_json::to_string(&rec).ok()
}

/// The turnover summary carried inside the slop record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TurnoverMetrics {
    /// `pass` | `fail` | `insufficient_sample` | `skipped`.
    pub verdict: String,
    pub mode: String,
    pub window_added: u64,
    pub window_commits: u64,
    pub ratios: turnover_core::window::Ratios,
    pub baseline_ratios: turnover_core::window::Ratios,
    #[serde(default)]
    pub failed_checks: Vec<String>,
}

/// Append a run record as one JSON line to `path`, creating it if needed.
pub fn append_jsonl(path: &Path, metrics: &RunMetrics) -> Result<()> {
    append_line(path, &metrics.to_json_line())
}

/// Append one pre-serialised JSON line to `path`, creating it if needed.
pub fn append_line(path: &Path, line: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating metrics directory {}", parent.display()))?;
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening metrics file {}", path.display()))?;
    writeln!(file, "{line}").with_context(|| format!("writing metrics to {}", path.display()))?;
    Ok(())
}

/// The connect/read/write timeout applied to the metrics POST. Bounded so a slow or unreachable
/// Mergestro `/v1/ingest` can never hang a customer's CI (mirrors Mergestro Queue's emitter).
pub const METRICS_POST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// POST a run record to a telemetry endpoint (best-effort; small + synchronous).
pub fn post(url: &str, token: Option<&str>, metrics: &RunMetrics) -> Result<()> {
    post_lines(url, token, &[metrics.to_json_line()])
}

/// POST several JSON-Lines records in ONE request. Mergestro's `/v1/ingest` takes a batch
/// and is idempotent per record, so the slop record and the turnover record travel together
/// and a retry never double-counts either.
pub fn post_lines(url: &str, token: Option<&str>, lines: &[String]) -> Result<()> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(METRICS_POST_TIMEOUT)
        .timeout_read(METRICS_POST_TIMEOUT)
        .timeout_write(METRICS_POST_TIMEOUT)
        .build();
    let mut req = agent
        .post(url)
        .set("Content-Type", "application/json")
        .set("User-Agent", "slop-gate");
    if let Some(token) = token {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    let mut body = lines.join("\n");
    body.push('\n');
    req.send_string(&body)
        .with_context(|| format!("posting metrics to {url}"))?;
    Ok(())
}

/// A stable identity for a survivor that tolerates line drift across an
/// evolving PR: it hashes the file and the mutation description, *not* the line
/// number (which shifts as the branch changes). This lets the analyser tell a
/// fixed survivor (disappears in a later run) from an overridden one (persists).
pub fn survivor_fingerprint(m: &Mutant) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    m.file.hash(&mut hasher);
    0u8.hash(&mut hasher); // separator so "ab"+"c" != "a"+"bc"
    m.description.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Best-effort PR number from `PR_NUMBER` then `GITHUB_REF` (`refs/pull/N/...`).
fn detect_pr_number() -> Option<u64> {
    if let Some(n) = env_nonempty("PR_NUMBER").and_then(|v| v.trim().parse().ok()) {
        return Some(n);
    }
    let r = env_nonempty("GITHUB_REF")?;
    let rest = r.strip_prefix("refs/pull/")?;
    rest.split_once('/')?.0.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::PreflightOutcome;

    fn mutant(file: &str, line: u32, desc: &str) -> Mutant {
        Mutant {
            file: file.into(),
            line,
            column: 1,
            function: None,
            description: desc.into(),
            name: format!("{file}:{line}:1: {desc}"),
        }
    }

    #[test]
    fn fingerprint_is_line_independent_but_file_and_desc_sensitive() {
        let a = survivor_fingerprint(&mutant("src/x.rs", 10, "replace > with >="));
        let b = survivor_fingerprint(&mutant("src/x.rs", 99, "replace > with >="));
        // Same file + description, different line → same fingerprint.
        assert_eq!(a, b);
        // Different file or description → different fingerprint.
        assert_ne!(
            a,
            survivor_fingerprint(&mutant("src/y.rs", 10, "replace > with >="))
        );
        assert_ne!(
            a,
            survivor_fingerprint(&mutant("src/x.rs", 10, "replace - with +"))
        );
    }

    #[test]
    fn turnover_lane_travels_as_a_summary_and_as_its_own_record_line() {
        use turnover_core::policy::{evaluate, Policy};
        use turnover_core::signals::Counts;
        use turnover_core::window::{Aggregate, Ratios};
        let agg = |added: u64, pasted: u64| {
            let counts = Counts {
                added,
                copy_pasted: pasted,
                ..Default::default()
            };
            Aggregate {
                counts,
                ratios: Ratios::of(&counts),
                commits: 2,
                authors: 1,
                ..Default::default()
            }
        };
        let window = agg(1000, 300);
        let baseline = agg(10_000, 800);
        let verdict = evaluate(&Policy::default(), Some(&baseline), &window);
        let mut report = GateReport::new("main", "HEAD");
        report.turnover = Some(crate::turnover_lane::TurnoverLane {
            status: "fail".into(),
            mode: "blocking".into(),
            reason: None,
            scope: "pr".into(),
            window_days: 90,
            head_sha: "lanehead".into(),
            window,
            baseline,
            has_baseline: true,
            failed_checks: vec!["copy_paste".into()],
            messages: vec!["copy_paste rose".into()],
            verdict: Some(verdict),
            markdown: String::new(),
            text: String::new(),
        });
        let ctx = RunContext {
            timestamp_unix: 1_760_000_000,
            repo: Some("acme/api".into()),
            pr: Some(7),
            head_sha: None,
            run_id: Some("r1".into()),
            actor: None,
            pr_author: Some("alice".into()),
        };
        let m = RunMetrics::from_report(&report, &Config::default(), &ctx);
        let t = m.turnover.as_ref().expect("summary present");
        assert_eq!(t.verdict, "fail");
        assert_eq!(t.window_added, 1000);
        assert_eq!(t.ratios.copy_paste, Some(0.3));
        assert_eq!(t.failed_checks, vec!["copy_paste"]);
        // The slop line still parses as a slop record with the summary embedded.
        let v: serde_json::Value = serde_json::from_str(&m.to_json_line()).unwrap();
        assert_eq!(v["record_type"], "slop");
        assert_eq!(v["turnover"]["verdict"], "fail");

        let line = turnover_record_line(&report, &ctx).expect("record line");
        let r: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(r["record_type"], "turnover");
        assert_eq!(r["repo"], "acme/api");
        assert_eq!(r["pr"], 7);
        assert_eq!(r["run_id"], "r1");
        assert_eq!(r["pr_author"], "alice");
        assert_eq!(
            r["head_sha"], "lanehead",
            "falls back to the lane's head when the env has none"
        );
        assert_eq!(r["verdict"], "fail");
        assert_eq!(r["window_days"], 90);
        assert_eq!(r["failed_checks"][0], "copy_paste");

        // A skipped lane emits no record: it decided nothing.
        let mut skipped = report.clone();
        skipped.turnover.as_mut().unwrap().verdict = None;
        assert!(turnover_record_line(&skipped, &ctx).is_none());
    }

    #[test]
    fn from_report_maps_mode_verdict_and_counts() {
        let mut report = GateReport::new("base", "head");
        report.changed_rust_files = vec!["src/x.rs".into()];
        report.preflight = PreflightOutcome::Passed { runs: 2 };
        report.tested = 4;
        report.caught = 3;
        report.survivors = vec![mutant("src/x.rs", 10, "replace > with >=")];
        report.verdict = Verdict::Block {
            reasons: vec!["1 surviving mutation(s) exceed the allowed 0".into()],
        };
        report.duration_secs = 12.5;

        let cfg = Config::default(); // block_on_survivors = true
        let ctx = RunContext {
            timestamp_unix: 1_700_000_000,
            repo: Some("octocat/hello".into()),
            pr: Some(42),
            head_sha: Some("deadbeef".into()),
            run_id: Some("99".into()),
            // The bot triggered the run; the human wrote the PR. Billing needs the latter.
            actor: Some("mergestro-bot".into()),
            pr_author: Some("octocat".into()),
        };
        let m = RunMetrics::from_report(&report, &cfg, &ctx);
        assert_eq!(m.mode, "blocking");
        assert_eq!(m.verdict, "block");
        assert_eq!(m.actor.as_deref(), Some("mergestro-bot"));
        assert_eq!(m.pr_author.as_deref(), Some("octocat"));
        assert_eq!(m.survivors, 1);
        assert_eq!(m.survivor_fingerprints.len(), 1);
        assert_eq!(m.pr, Some(42));
        assert_eq!(m.tested, 4);
        assert_eq!(m.duration_secs, 12.5);

        // Advisory config flips the mode.
        let advisory = Config {
            block_on_survivors: false,
            ..Config::default()
        };
        assert_eq!(
            RunMetrics::from_report(&report, &advisory, &ctx).mode,
            "advisory"
        );
    }

    #[test]
    fn json_line_round_trips() {
        let report = GateReport::new("base", "head");
        let m = RunMetrics::from_report(&report, &Config::default(), &RunContext::default());
        let line = m.to_json_line();
        assert!(!line.contains('\n'));
        let back: RunMetrics = serde_json::from_str(&line).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn record_carries_slop_ingest_discriminator() {
        // The line the gate emits must drop straight into Mergestro's ingest envelope:
        // `records.rs::IngestRecord` is internally tagged on `record_type`, so a missing/other tag
        // makes `/v1/ingest` reject the upload with "missing field `record_type`". Keep this in
        // lockstep with the EE-side contract test in `ee/mergestro/src/records.rs`
        // (`gate_wire_line_parses_into_slop_ingest_record`).
        let report = GateReport::new("base", "head");
        let m = RunMetrics::from_report(&report, &Config::default(), &RunContext::default());
        assert_eq!(m.record_type, "slop");
        let line = m.to_json_line();
        assert!(
            line.contains("\"record_type\":\"slop\""),
            "emitted line must carry the slop discriminator: {line}"
        );
    }

    #[test]
    fn pre_v6_record_without_record_type_still_parses() {
        // A v5 line (no `record_type`) written before this field existed must still deserialize so
        // `crate::analyze` can read historical local telemetry; the tag defaults to "slop".
        let v5 = r#"{"schema_version":5,"gate_version":"0.4.0","timestamp_unix":1,
            "mode":"blocking","verdict":"pass","block_reasons":[],"changed_rust_files":0,
            "candidates":0,"capped_out":0,"tested":0,"caught":0,"survivors":0,"timed_out":0,
            "unviable":0,"survivor_fingerprints":[],"zero_assertion_tests":0,"duration_secs":0.0}"#;
        let m: RunMetrics = serde_json::from_str(v5).unwrap();
        assert_eq!(m.record_type, "slop");
    }

    #[test]
    fn v1_record_parses_with_phase4_defaults() {
        // A schema-v1 line (no Phase 4 fields) must still deserialize, defaulting
        // the new fields so older telemetry stays readable.
        let v1 = r#"{"schema_version":1,"gate_version":"0.3.0","timestamp_unix":1,
            "mode":"blocking","verdict":"pass","block_reasons":[],"changed_rust_files":0,
            "candidates":0,"capped_out":0,"tested":0,"caught":0,"survivors":0,"timed_out":0,
            "unviable":0,"survivor_fingerprints":[],"zero_assertion_tests":0,"duration_secs":0.0}"#;
        let m: RunMetrics = serde_json::from_str(v1).unwrap();
        assert_eq!(m.mutation_score, None);
        assert_eq!(
            m.severity_counts,
            crate::severity::SeverityCounts::default()
        );
        assert_eq!(m.debt_score, None);
    }

    #[test]
    fn from_report_computes_mutation_score_and_severity() {
        let mut report = GateReport::new("base", "head");
        report.changed_rust_files = vec!["src/auth.rs".into()];
        report.preflight = PreflightOutcome::Passed { runs: 1 };
        report.tested = 4;
        report.caught = 3;
        report.survivors = vec![mutant("src/auth.rs", 10, "replace > with >=")]; // critical
        report.debt = Some(crate::debt::DebtDelta {
            complexity: 5,
            ..crate::debt::DebtDelta::default()
        });
        let m = RunMetrics::from_report(&report, &Config::default(), &RunContext::default());
        assert_eq!(m.mutation_score, Some(0.75));
        assert_eq!(m.severity_counts.critical, 1);
        assert_eq!(m.debt_score, Some(5));
    }

    #[test]
    fn append_jsonl_writes_one_line_per_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/metrics.jsonl");
        let report = GateReport::new("base", "head");
        let m = RunMetrics::from_report(&report, &Config::default(), &RunContext::default());
        append_jsonl(&path, &m).unwrap();
        append_jsonl(&path, &m).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 2);
    }
}
