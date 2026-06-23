// SPDX-License-Identifier: Apache-2.0
//! Phase 3 — read the validation telemetry back into the headline KPIs.
//!
//! Given the JSON-Lines stream that the gate emits ([`crate::metrics`]), this
//! computes the numbers the Phase 3 exit gate is judged on:
//!
//! - **blocking vs advisory** mix, and how often a blocking run actually blocks,
//! - **survivors acted on** — the fix-vs-override rate, derived by tracking each
//!   survivor's fingerprint across the runs of a single PR over time,
//! - **latency** tolerance (mean / p50 / p95),
//! - reach (distinct repos / PRs) as a retention proxy.
//!
//! "Disable rate" and "kept enabled ≥ 1 month" can't be read from the runs
//! alone (a disabled gate emits nothing); they're judged from the *trend* of
//! these records over calendar time, which is an ingestion-side concern.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::metrics::RunMetrics;
use crate::severity::SeverityCounts;

/// Load a JSON-Lines metrics file into records, skipping blank lines.
pub fn load(path: &Path) -> Result<Vec<RunMetrics>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading metrics file {}", path.display()))?;
    parse(&text)
}

/// Parse JSON-Lines text into records. Each non-empty line must be one record.
pub fn parse(text: &str) -> Result<Vec<RunMetrics>> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: RunMetrics = serde_json::from_str(line)
            .with_context(|| format!("parsing metrics line {}", i + 1))?;
        out.push(rec);
    }
    Ok(out)
}

/// The headline validation KPIs computed over a set of runs.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationSummary {
    pub total_runs: usize,
    pub distinct_repos: usize,
    pub distinct_prs: usize,

    pub blocking_runs: usize,
    pub advisory_runs: usize,
    /// Of the blocking-mode runs, how many actually blocked.
    pub blocked_runs: usize,

    pub runs_with_survivors: usize,
    pub total_survivors: usize,
    pub total_zero_assertion: usize,

    /// Distinct survivors (per PR) that disappeared in a later run of the same
    /// PR — i.e. acted on.
    pub survivors_fixed: usize,
    /// Distinct survivors (per PR) still present in that PR's most recent run.
    pub survivors_carried: usize,

    pub latency_mean_secs: f64,
    pub latency_p50_secs: f64,
    pub latency_p95_secs: f64,

    // ── Phase 4: rollout trend ──────────────────────────────────────────────
    /// Mean mutation score (`caught / tested`) across runs that tested anything.
    pub mutation_score_mean: f64,
    /// Trend in mutation score over calendar time: the later half's mean minus
    /// the earlier half's (positive = the suite is getting better at catching).
    pub mutation_score_trend: f64,
    /// Survivor severity mix summed across all runs.
    pub severity: SeverityCounts,
    /// Net structural debt added across all runs (sum of per-run debt scores).
    pub debt_total: i64,

    // ── Pattern lanes (Track B) ─────────────────────────────────────────────
    /// Mean slop score across runs that scored one. The Track C KPI under test.
    pub slop_score_mean: f64,
    /// Total slop signatures flagged across all runs.
    pub total_slop_findings: usize,
    /// Mean security anti-pattern score across runs that scored one.
    pub security_score_mean: f64,
    /// Total security anti-patterns flagged across all runs.
    pub total_security_findings: usize,
    /// Mean convention/hallucinated-import score across runs that scored one.
    pub convention_score_mean: f64,
    /// Total unknown-crate imports flagged across all runs.
    pub total_convention_findings: usize,
}

impl ValidationSummary {
    /// Block rate among blocking-mode runs (`None` if there were none).
    pub fn block_rate(&self) -> Option<f64> {
        if self.blocking_runs == 0 {
            None
        } else {
            Some(self.blocked_runs as f64 / self.blocking_runs as f64)
        }
    }

    /// Fraction of distinct survivors that were acted on (`None` if none seen).
    pub fn fix_rate(&self) -> Option<f64> {
        let total = self.survivors_fixed + self.survivors_carried;
        if total == 0 {
            None
        } else {
            Some(self.survivors_fixed as f64 / total as f64)
        }
    }

    /// Render a compact human-readable summary.
    pub fn render_text(&self) -> String {
        let pct = |o: Option<f64>| match o {
            Some(v) => format!("{:.0}%", v * 100.0),
            None => "n/a".to_string(),
        };
        let mut out = String::new();
        out.push_str("── Slop Filter · validation & trend summary (Phase 3–4) ──\n");
        out.push_str(&format!(
            "runs:       {} across {} repo(s), {} PR(s)\n",
            self.total_runs, self.distinct_repos, self.distinct_prs
        ));
        out.push_str(&format!(
            "mode:       {} blocking · {} advisory\n",
            self.blocking_runs, self.advisory_runs
        ));
        out.push_str(&format!(
            "block rate: {} ({} of {} blocking runs blocked)\n",
            pct(self.block_rate()),
            self.blocked_runs,
            self.blocking_runs
        ));
        out.push_str(&format!(
            "survivors:  {} total, in {} run(s)\n",
            self.total_survivors, self.runs_with_survivors
        ));
        out.push_str(&format!(
            "acted-on:   {} ({} fixed, {} carried)\n",
            pct(self.fix_rate()),
            self.survivors_fixed,
            self.survivors_carried
        ));
        out.push_str(&format!(
            "no-assert:  {} test(s) flagged\n",
            self.total_zero_assertion
        ));
        out.push_str(&format!(
            "latency:    mean {:.1}s · p50 {:.1}s · p95 {:.1}s\n",
            self.latency_mean_secs, self.latency_p50_secs, self.latency_p95_secs
        ));
        // ±0.5% is treated as noise (the metric is a 0..1 ratio); only a larger
        // shift in either direction is called out as a real trend.
        let direction = if self.mutation_score_trend > 0.005 {
            "improving"
        } else if self.mutation_score_trend < -0.005 {
            "declining"
        } else {
            "flat"
        };
        out.push_str(&format!(
            "mut-score:  mean {:.0}% · trend {:+.0}% ({})\n",
            self.mutation_score_mean * 100.0,
            self.mutation_score_trend * 100.0,
            direction
        ));
        out.push_str(&format!(
            "severity:   {} critical · {} high · {} medium · {} low\n",
            self.severity.critical, self.severity.high, self.severity.medium, self.severity.low
        ));
        out.push_str(&format!(
            "debt-delta: {:+} net across all runs\n",
            self.debt_total
        ));
        out.push_str(&format!(
            "slop:       mean score {:.0}/100 · {} signature(s) flagged\n",
            self.slop_score_mean, self.total_slop_findings
        ));
        out.push_str(&format!(
            "security:   mean score {:.0}/100 · {} anti-pattern(s) flagged\n",
            self.security_score_mean, self.total_security_findings
        ));
        out.push_str(&format!(
            "convention: mean score {:.0}/100 · {} unknown import(s) flagged\n",
            self.convention_score_mean, self.total_convention_findings
        ));
        out
    }
}

/// Compute the headline KPIs over `runs`.
pub fn summarize(runs: &[RunMetrics]) -> ValidationSummary {
    let mut repos = std::collections::BTreeSet::new();
    let mut blocking_runs = 0;
    let mut advisory_runs = 0;
    let mut blocked_runs = 0;
    let mut runs_with_survivors = 0;
    let mut total_survivors = 0;
    let mut total_zero_assertion = 0;
    let mut durations = Vec::with_capacity(runs.len());
    let mut severity = SeverityCounts::default();
    let mut debt_total: i64 = 0;
    let mut slop_scores: Vec<f64> = Vec::new();
    let mut total_slop_findings = 0usize;
    let mut security_scores: Vec<f64> = Vec::new();
    let mut total_security_findings = 0usize;
    let mut convention_scores: Vec<f64> = Vec::new();
    let mut total_convention_findings = 0usize;
    // (timestamp, mutation_score) for runs that tested something — ordered later
    // to read the score trend over calendar time.
    let mut scored: Vec<(u64, f64)> = Vec::new();

    // Group runs by PR (repo + pr number) to trace survivors over time.
    let mut by_pr: BTreeMap<(String, u64), Vec<&RunMetrics>> = BTreeMap::new();

    for r in runs {
        if let Some(repo) = &r.repo {
            repos.insert(repo.clone());
        }
        if r.mode == "blocking" {
            blocking_runs += 1;
            if r.verdict == "block" {
                blocked_runs += 1;
            }
        } else {
            advisory_runs += 1;
        }
        if r.survivors > 0 {
            runs_with_survivors += 1;
        }
        total_survivors += r.survivors;
        total_zero_assertion += r.zero_assertion_tests;
        durations.push(r.duration_secs);
        severity.merge(&r.severity_counts);
        debt_total += r.debt_score.unwrap_or(0);
        total_slop_findings += r.slop_findings;
        if let Some(s) = r.slop_score {
            slop_scores.push(s as f64);
        }
        total_security_findings += r.security_findings;
        if let Some(s) = r.security_score {
            security_scores.push(s as f64);
        }
        total_convention_findings += r.convention_findings;
        if let Some(s) = r.convention_score {
            convention_scores.push(s as f64);
        }
        if let Some(score) = r.mutation_score {
            scored.push((r.timestamp_unix, score));
        }

        if let (Some(repo), Some(pr)) = (r.repo.clone(), r.pr) {
            by_pr.entry((repo, pr)).or_default().push(r);
        }
    }

    let (survivors_fixed, survivors_carried) = fix_vs_carried(&by_pr);
    let (mutation_score_mean, mutation_score_trend) = score_mean_and_trend(&mut scored);

    ValidationSummary {
        total_runs: runs.len(),
        distinct_repos: repos.len(),
        distinct_prs: by_pr.len(),
        blocking_runs,
        advisory_runs,
        blocked_runs,
        runs_with_survivors,
        total_survivors,
        total_zero_assertion,
        survivors_fixed,
        survivors_carried,
        latency_mean_secs: mean(&durations),
        latency_p50_secs: percentile(&durations, 0.50),
        latency_p95_secs: percentile(&durations, 0.95),
        mutation_score_mean,
        mutation_score_trend,
        severity,
        debt_total,
        slop_score_mean: mean(&slop_scores),
        total_slop_findings,
        security_score_mean: mean(&security_scores),
        total_security_findings,
        convention_score_mean: mean(&convention_scores),
        total_convention_findings,
    }
}

/// Mean mutation score and its trend over time. `scored` is `(timestamp, score)`
/// for runs that tested something; it is sorted in place by timestamp. The trend
/// is the later half's mean minus the earlier half's (0.0 with fewer than two
/// points), so a rising score reads positive.
fn score_mean_and_trend(scored: &mut [(u64, f64)]) -> (f64, f64) {
    if scored.is_empty() {
        return (0.0, 0.0);
    }
    let values: Vec<f64> = scored.iter().map(|(_, s)| *s).collect();
    let overall = mean(&values);
    if scored.len() < 2 {
        return (overall, 0.0);
    }
    scored.sort_by_key(|(ts, _)| *ts);
    let mid = scored.len() / 2;
    let early: Vec<f64> = scored[..mid].iter().map(|(_, s)| *s).collect();
    let late: Vec<f64> = scored[mid..].iter().map(|(_, s)| *s).collect();
    (overall, mean(&late) - mean(&early))
}

/// For each PR, a survivor fingerprint seen in any run but absent from that PR's
/// most recent run counts as **fixed**; one present in the most recent run
/// counts as **carried** (overridden / not yet addressed).
fn fix_vs_carried(by_pr: &BTreeMap<(String, u64), Vec<&RunMetrics>>) -> (usize, usize) {
    let mut fixed = 0;
    let mut carried = 0;
    for runs in by_pr.values() {
        // Order this PR's runs in time; the last is the current state.
        let mut runs = runs.clone();
        runs.sort_by_key(|r| r.timestamp_unix);
        let Some(last) = runs.last() else { continue };
        let in_last: std::collections::BTreeSet<&String> =
            last.survivor_fingerprints.iter().collect();
        let ever: std::collections::BTreeSet<&String> = runs
            .iter()
            .flat_map(|r| r.survivor_fingerprints.iter())
            .collect();
        for fp in ever {
            if in_last.contains(fp) {
                carried += 1;
            } else {
                fixed += 1;
            }
        }
    }
    (fixed, carried)
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

/// Nearest-rank percentile (`q` in `0.0..=1.0`).
fn percentile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = (q * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::metrics::{RunContext, RunMetrics};
    use crate::report::{GateReport, Mutant, Verdict};

    fn run(repo: &str, pr: u64, ts: u64, survivor_descs: &[&str], blocking: bool) -> RunMetrics {
        let mut report = GateReport::new("base", "head");
        report.changed_rust_files = vec!["src/x.rs".into()];
        report.survivors = survivor_descs
            .iter()
            .map(|d| Mutant {
                file: "src/x.rs".into(),
                line: 1,
                column: 1,
                function: None,
                description: (*d).into(),
                name: format!("src/x.rs:1:1: {d}"),
            })
            .collect();
        if !report.survivors.is_empty() {
            report.verdict = Verdict::Block {
                reasons: vec!["survivors".into()],
            };
        }
        let cfg = Config {
            block_on_survivors: blocking,
            ..Config::default()
        };
        let ctx = RunContext {
            timestamp_unix: ts,
            repo: Some(repo.into()),
            pr: Some(pr),
            ..RunContext::default()
        };
        RunMetrics::from_report(&report, &cfg, &ctx)
    }

    #[test]
    fn parse_skips_blank_lines() {
        let r = run("o/r", 1, 1, &[], true);
        let text = format!("{}\n\n{}\n", r.to_json_line(), r.to_json_line());
        assert_eq!(parse(&text).unwrap().len(), 2);
    }

    #[test]
    fn fix_vs_override_tracks_survivors_across_a_pr() {
        // PR #1: run1 has survivors A and B; run2 (later) only B remains.
        // → A fixed, B carried.
        let runs = vec![
            run("o/r", 1, 100, &["mut A", "mut B"], true),
            run("o/r", 1, 200, &["mut B"], true),
        ];
        let s = summarize(&runs);
        assert_eq!(s.survivors_fixed, 1);
        assert_eq!(s.survivors_carried, 1);
        assert_eq!(s.fix_rate(), Some(0.5));
        assert_eq!(s.distinct_prs, 1);
    }

    #[test]
    fn block_rate_counts_only_blocking_mode() {
        let runs = vec![
            run("o/r", 1, 100, &["mut A"], true),  // blocking, blocked
            run("o/r", 2, 100, &[], true),         // blocking, passed
            run("o/r", 3, 100, &["mut C"], false), // advisory (not counted)
        ];
        let s = summarize(&runs);
        assert_eq!(s.blocking_runs, 2);
        assert_eq!(s.advisory_runs, 1);
        assert_eq!(s.blocked_runs, 1);
        assert_eq!(s.block_rate(), Some(0.5));
    }

    #[test]
    fn empty_input_is_well_defined() {
        let s = summarize(&[]);
        assert_eq!(s.total_runs, 0);
        assert_eq!(s.block_rate(), None);
        assert_eq!(s.fix_rate(), None);
        assert_eq!(s.latency_p95_secs, 0.0);
    }

    /// Build a run with an explicit mutation score and one severity-bearing
    /// survivor, for the Phase 4 trend/severity aggregation.
    fn scored_run(ts: u64, tested: usize, caught: usize, survivor: &str) -> RunMetrics {
        let mut report = GateReport::new("base", "head");
        report.changed_rust_files = vec!["src/auth.rs".into()];
        report.tested = tested;
        report.caught = caught;
        report.survivors = vec![Mutant {
            file: "src/auth.rs".into(),
            line: 1,
            column: 1,
            function: None,
            description: survivor.into(),
            name: format!("src/auth.rs:1:1: {survivor}"),
        }];
        let ctx = RunContext {
            timestamp_unix: ts,
            repo: Some("o/r".into()),
            pr: Some(1),
            ..RunContext::default()
        };
        RunMetrics::from_report(&report, &Config::default(), &ctx)
    }

    #[test]
    fn mutation_score_trend_and_severity_aggregate() {
        // Score climbs 0.5 → 0.9 over time; survivor is a critical auth flip.
        let runs = vec![
            scored_run(100, 4, 2, "replace > with >="),  // score 0.50
            scored_run(200, 10, 9, "replace > with >="), // score 0.90
        ];
        let s = summarize(&runs);
        assert!((s.mutation_score_mean - 0.70).abs() < 1e-9);
        assert!(
            s.mutation_score_trend > 0.0,
            "score should read as improving"
        );
        assert_eq!(s.severity.critical, 2);
        let text = s.render_text();
        assert!(text.contains("mut-score:"));
        assert!(text.contains("improving"));
    }

    #[test]
    fn percentile_nearest_rank() {
        let v = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile(&v, 0.0), 1.0);
        assert_eq!(percentile(&v, 0.5), 3.0);
        assert_eq!(percentile(&v, 1.0), 5.0);
    }

    #[test]
    fn total_slop_findings_accumulates_across_runs() {
        use crate::metrics::RunContext;
        use crate::pattern::{PatternFinding, PatternReport};

        let mut r1 = GateReport::new("base", "head");
        r1.slop = Some(PatternReport {
            findings: vec![PatternFinding {
                rule: "redundant-wrapper".into(),
                file: "src/x.rs".into(),
                line: 1,
                message: "wraps foo".into(),
                weight: 25,
            }],
            score: 25,
        });

        let mut r2 = GateReport::new("base", "head");
        r2.slop = Some(PatternReport {
            findings: vec![
                PatternFinding {
                    rule: "tautological-assert".into(),
                    file: "src/y.rs".into(),
                    line: 1,
                    message: "assert!(true) can never fail".into(),
                    weight: 20,
                },
                PatternFinding {
                    rule: "over-commented".into(),
                    file: "src/y.rs".into(),
                    line: 5,
                    message: "75% comments".into(),
                    weight: 15,
                },
            ],
            score: 35,
        });

        let ctx = RunContext::default();
        let m1 = crate::metrics::RunMetrics::from_report(&r1, &Config::default(), &ctx);
        let m2 = crate::metrics::RunMetrics::from_report(&r2, &Config::default(), &ctx);
        let s = summarize(&[m1, m2]);
        // r1 has 1 finding, r2 has 2 — the accumulation must be additive (+= not -= or *=).
        assert_eq!(s.total_slop_findings, 3, "total_slop_findings must sum across runs");
    }

    #[test]
    fn total_security_findings_accumulates_across_runs() {
        use crate::metrics::RunContext;
        use crate::pattern::{PatternFinding, PatternReport};

        let mut r1 = GateReport::new("base", "head");
        r1.security = Some(PatternReport {
            findings: vec![PatternFinding {
                rule: "hardcoded-secret".into(),
                file: "src/x.rs".into(),
                line: 1,
                message: "hardcoded credential".into(),
                weight: 40,
            }],
            score: 40,
        });

        let mut r2 = GateReport::new("base", "head");
        r2.security = Some(PatternReport {
            findings: vec![
                PatternFinding {
                    rule: "weak-hash".into(),
                    file: "src/y.rs".into(),
                    line: 5,
                    message: "MD5 is weak".into(),
                    weight: 25,
                },
                PatternFinding {
                    rule: "shell-command".into(),
                    file: "src/y.rs".into(),
                    line: 8,
                    message: "shell spawn".into(),
                    weight: 20,
                },
            ],
            score: 45,
        });

        let ctx = RunContext::default();
        let m1 = crate::metrics::RunMetrics::from_report(&r1, &Config::default(), &ctx);
        let m2 = crate::metrics::RunMetrics::from_report(&r2, &Config::default(), &ctx);
        let s = summarize(&[m1, m2]);
        // r1 has 1 finding, r2 has 2 — the accumulation must be additive (+= not -= or *=).
        assert_eq!(
            s.total_security_findings,
            3,
            "total_security_findings must sum across runs"
        );
    }

    #[test]
    fn total_convention_findings_accumulates_across_runs() {
        use crate::metrics::RunContext;
        use crate::pattern::{PatternFinding, PatternReport};

        let mut r1 = GateReport::new("base", "head");
        r1.convention = Some(PatternReport {
            findings: vec![PatternFinding {
                rule: "unknown-crate-import".into(),
                file: "src/x.rs".into(),
                line: 1,
                message: "ghost is not a declared dep".into(),
                weight: 30,
            }],
            score: 30,
        });

        let mut r2 = GateReport::new("base", "head");
        r2.convention = Some(PatternReport {
            findings: vec![
                PatternFinding {
                    rule: "unknown-crate-import".into(),
                    file: "src/y.rs".into(),
                    line: 2,
                    message: "phantom is not a declared dep".into(),
                    weight: 30,
                },
                PatternFinding {
                    rule: "unknown-crate-import".into(),
                    file: "src/y.rs".into(),
                    line: 5,
                    message: "fake_lib is not a declared dep".into(),
                    weight: 30,
                },
            ],
            score: 60,
        });

        let ctx = RunContext::default();
        let m1 = crate::metrics::RunMetrics::from_report(&r1, &Config::default(), &ctx);
        let m2 = crate::metrics::RunMetrics::from_report(&r2, &Config::default(), &ctx);
        let s = summarize(&[m1, m2]);
        // r1 has 1 finding, r2 has 2 — the accumulation must be additive (+= not -= or *=).
        assert_eq!(
            s.total_convention_findings,
            3,
            "total_convention_findings must sum across runs"
        );
    }
}
