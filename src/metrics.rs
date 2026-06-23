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
/// (`convention_score`, `convention_findings`).
pub const SCHEMA_VERSION: u32 = 5;

/// One run's worth of validation telemetry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunMetrics {
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
            schema_version: SCHEMA_VERSION,
            gate_version: env!("CARGO_PKG_VERSION").to_string(),
            timestamp_unix: ctx.timestamp_unix,
            repo: ctx.repo.clone(),
            pr: ctx.pr,
            head_sha: ctx.head_sha.clone(),
            run_id: ctx.run_id.clone(),
            actor: ctx.actor.clone(),
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
        }
    }
}

/// Append a run record as one JSON line to `path`, creating it if needed.
pub fn append_jsonl(path: &Path, metrics: &RunMetrics) -> Result<()> {
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
    writeln!(file, "{}", metrics.to_json_line())
        .with_context(|| format!("writing metrics to {}", path.display()))?;
    Ok(())
}

/// POST a run record to a telemetry endpoint (best-effort; small + synchronous).
pub fn post(url: &str, token: Option<&str>, metrics: &RunMetrics) -> Result<()> {
    let agent = ureq::AgentBuilder::new().build();
    let mut req = agent
        .post(url)
        .set("Content-Type", "application/json")
        .set("User-Agent", "slop-gate");
    if let Some(token) = token {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    req.send_string(&metrics.to_json_line())
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
            actor: Some("octocat".into()),
        };
        let m = RunMetrics::from_report(&report, &cfg, &ctx);
        assert_eq!(m.mode, "blocking");
        assert_eq!(m.verdict, "block");
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
