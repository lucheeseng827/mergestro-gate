// SPDX-License-Identifier: Apache-2.0
//! The wire record a repository uploads so the Mergestro console can draw its
//! tree — `record_type: "progression"`, one JSON line on `POST /v1/ingest`.
//!
//! Shaped like every other ingest record: identity first (`repo`, `pr`,
//! `head_sha`, `run_id`, `actor`, `pr_author`, `timestamp_unix`), payload
//! after. Identity is the idempotency key, so re-uploading the same resolve is
//! a no-op and a retried CI step is safe.
//!
//! A snapshot is *latest-wins* rather than a trend point: the console asks
//! "where is this repo now", and the answer is the newest resolve. The plane
//! keeps the older ones because the same rows also answer "when did that
//! milestone close", which is the one question a repo checkout cannot answer
//! once the plan has moved on.

use serde::{Deserialize, Serialize};

use super::resolve::{NodeProgress, ProgressionSnapshot, ProgressionTotals};

/// Who and when — filled from the CI environment.
#[derive(Debug, Clone, Default)]
pub struct Identity {
    pub repo: String,
    pub pr: Option<u64>,
    pub head_sha: String,
    pub run_id: String,
    pub actor: Option<String>,
    pub pr_author: Option<String>,
    pub timestamp_unix: u64,
}

/// One `progression` ingest record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressionRecord {
    pub record_type: &'static str,
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    pub head_sha: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_author: Option<String>,
    pub timestamp_unix: u64,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season: Option<String>,
    pub totals: ProgressionTotals,
    pub nodes: Vec<NodeProgress>,
    pub tool_version: String,
}

/// Build the record for a resolved snapshot.
///
/// `head_sha` comes from the identity when CI knows it (a PR's head is not the
/// checked-out merge commit) and falls back to what the resolve actually saw.
pub fn build(identity: Identity, snap: &ProgressionSnapshot) -> ProgressionRecord {
    ProgressionRecord {
        record_type: "progression",
        repo: identity.repo,
        pr: identity.pr,
        head_sha: if identity.head_sha.is_empty() {
            snap.head_sha.clone()
        } else {
            identity.head_sha
        },
        run_id: identity.run_id,
        actor: identity.actor,
        pr_author: identity.pr_author,
        timestamp_unix: if identity.timestamp_unix == 0 {
            snap.generated_at_unix
        } else {
            identity.timestamp_unix
        },
        title: snap.title.clone(),
        season: snap.season.clone(),
        totals: snap.totals.clone(),
        nodes: snap.nodes.clone(),
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// The record as one JSON line, ready for `POST /v1/ingest`.
pub fn to_json_line(rec: &ProgressionRecord) -> String {
    serde_json::to_string(rec).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progression::resolve::ProgressionTotals;

    fn snap() -> ProgressionSnapshot {
        ProgressionSnapshot {
            version: 1,
            title: "plan".into(),
            season: None,
            generated_at_unix: 1_700_000_000,
            head_sha: "resolved".into(),
            since_days: None,
            totals: ProgressionTotals {
                level: 2,
                ..Default::default()
            },
            nodes: Vec::new(),
        }
    }

    #[test]
    fn identity_wins_over_the_resolve_but_never_leaves_a_hole() {
        let rec = build(
            Identity {
                repo: "acme/api".into(),
                head_sha: "ci-head".into(),
                run_id: "r1".into(),
                timestamp_unix: 42,
                ..Default::default()
            },
            &snap(),
        );
        assert_eq!(rec.head_sha, "ci-head");
        assert_eq!(rec.timestamp_unix, 42);

        // Locally there is no CI environment, and a record with an empty head
        // would be un-dedupable rather than merely unlabelled.
        let local = build(
            Identity {
                repo: "acme/api".into(),
                run_id: "local".into(),
                ..Default::default()
            },
            &snap(),
        );
        assert_eq!(local.head_sha, "resolved");
        assert_eq!(local.timestamp_unix, 1_700_000_000);
    }

    #[test]
    fn the_line_is_tagged_so_one_ingest_can_discriminate_it() {
        let line = to_json_line(&build(
            Identity {
                repo: "acme/api".into(),
                head_sha: "h".into(),
                run_id: "r".into(),
                timestamp_unix: 1,
                ..Default::default()
            },
            &snap(),
        ));
        assert!(
            line.starts_with(r#"{"record_type":"progression","repo":"acme/api""#),
            "{line}"
        );
        assert!(!line.contains('\n'));
    }
}
