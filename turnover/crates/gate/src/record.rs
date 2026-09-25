//! The `turnover` record on Mergestro's ingest wire (`record_type` envelope, one JSON object
//! per line). Identity fields mirror the other gates so the control plane's idempotency key
//! (`kind:repo:head_sha:run_id`) and per-active-developer attribution (`pr_author`) work
//! unchanged. Everything the fleet trend view needs travels in the record: the window and
//! baseline counts, both ratio sets, and the verdict — so the paid plane never re-walks a
//! customer's history, it only stores what the free gate already computed.

use std::collections::BTreeMap;

use serde::Serialize;
use turnover_core::policy::{Status, Verdict};
use turnover_core::signals::Counts;
use turnover_core::window::{Aggregate, Ratios};

#[derive(Debug, Clone, Serialize)]
pub struct TurnoverRecord {
    pub record_type: &'static str,
    pub repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    pub head_sha: String,
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_author: Option<String>,
    pub timestamp_unix: u64,
    pub mode: &'static str,
    /// `pass` | `fail` | `insufficient_sample`.
    pub verdict: &'static str,
    pub window_days: u32,
    pub window: Counts,
    pub window_commits: u64,
    pub baseline: Counts,
    pub baseline_commits: u64,
    pub ratios: Ratios,
    pub baseline_ratios: Ratios,
    pub failed_checks: Vec<String>,
    pub languages: BTreeMap<String, Ratios>,
    /// `"ai"` / `"human"` slices of the window: commits, added lines and ratios.
    pub origins: BTreeMap<String, OriginSlice>,
    pub tool_version: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct OriginSlice {
    pub commits: u64,
    pub added: u64,
    pub ratios: Ratios,
}

pub struct Identity {
    pub repo: String,
    pub pr: Option<u64>,
    pub head_sha: String,
    pub run_id: String,
    pub actor: Option<String>,
    pub pr_author: Option<String>,
    pub timestamp_unix: u64,
}

pub fn build(
    id: Identity,
    window_days: u32,
    verdict: &Verdict,
    window: &Aggregate,
    baseline: &Aggregate,
) -> TurnoverRecord {
    TurnoverRecord {
        record_type: "turnover",
        repo: id.repo,
        pr: id.pr,
        head_sha: id.head_sha,
        run_id: id.run_id,
        actor: id.actor,
        pr_author: id.pr_author,
        timestamp_unix: id.timestamp_unix,
        mode: verdict.mode.as_str(),
        verdict: match verdict.status {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::InsufficientSample => "insufficient_sample",
        },
        window_days,
        window: window.counts,
        window_commits: window.commits,
        baseline: baseline.counts,
        baseline_commits: baseline.commits,
        ratios: window.ratios,
        baseline_ratios: baseline.ratios,
        failed_checks: verdict.failed_checks().map(|c| c.signal.clone()).collect(),
        languages: window
            .by_language
            .iter()
            .map(|(k, v)| (k.clone(), v.ratios))
            .collect(),
        origins: window
            .by_origin
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    OriginSlice {
                        commits: v.commits,
                        added: v.counts.added,
                        ratios: v.ratios,
                    },
                )
            })
            .collect(),
        tool_version: env!("CARGO_PKG_VERSION"),
    }
}
