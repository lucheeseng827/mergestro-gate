//! Churn attribution: added lines that were removed again within the horizon. GitClear's
//! published definition is two weeks; the horizon is a parameter so a repository can measure
//! the 30/90-day "code turnover" variant with the same pass.
//!
//! This is the one signal that needs commits in time order, so it runs after the parallel
//! walk over [`CommitSignals`] rows that still carry their fingerprints. A fingerprint is
//! `hash(path, normalised line)`; a deletion of that fingerprint within `horizon_secs` of an
//! addition consumes the addition (1:1, oldest unexpired first) and charges `churned` to the
//! commit that added it. Moved lines were never fingerprinted as additions, so a refactor
//! does not churn itself.
//!
//! ## Refresh boundary
//!
//! A baseline is refreshed incrementally, and the rows already in it no longer carry
//! fingerprints. So every pass returns the additions that are still young enough to be
//! churned by a *future* commit, and the next pass seeds its pool from them. Without this the
//! last two weeks before every refresh would be blind to churn — precisely the lines a
//! gate run cares about most.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::signals::CommitSignals;

/// Two weeks, the published churn window.
pub const DEFAULT_HORIZON_SECS: i64 = 14 * 24 * 3600;

/// An addition that has not been deleted yet and is still inside the horizon at the end of
/// a pass. Persisted in the baseline so the next refresh can consume it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingAddition {
    pub sha: String,
    pub path: String,
    pub fingerprint: u64,
    pub timestamp_unix: i64,
}

/// Fill `counts.churned` on every row, seeding the pool from `pending`. Rows may arrive in
/// any order; they are processed by `timestamp_unix` ascending (ties by sha). Returns the
/// additions still eligible to be churned after the newest row.
pub fn attribute(
    rows: &mut [CommitSignals],
    pending: &[PendingAddition],
    horizon_secs: i64,
) -> Vec<PendingAddition> {
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&a, &b| {
        rows[a]
            .timestamp_unix
            .cmp(&rows[b].timestamp_unix)
            .then_with(|| rows[a].sha.cmp(&rows[b].sha))
    });
    let by_sha: HashMap<&str, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| (r.sha.as_str(), i))
        .collect();

    // (path, fingerprint) -> additions awaiting a deletion: (timestamp, adder). The adder is a
    // row index when the row is in this slice, else the sha of a row that is not (a pending
    // addition whose commit was dropped) — charged nowhere, but still consumed 1:1.
    #[derive(Clone)]
    enum Adder {
        Row(usize),
        Gone,
    }
    let mut pool: HashMap<(String, u64), VecDeque<(i64, Adder)>> = HashMap::new();
    let mut seeds: Vec<&PendingAddition> = pending.iter().collect();
    seeds.sort_by(|a, b| {
        a.timestamp_unix
            .cmp(&b.timestamp_unix)
            .then_with(|| a.sha.cmp(&b.sha))
    });
    for p in seeds {
        let adder = by_sha
            .get(p.sha.as_str())
            .map(|&i| Adder::Row(i))
            .unwrap_or(Adder::Gone);
        pool.entry((p.path.clone(), p.fingerprint))
            .or_default()
            .push_back((p.timestamp_unix, adder));
    }

    let mut newest = i64::MIN;
    for &i in &order {
        let now = rows[i].timestamp_unix;
        newest = newest.max(now);
        // Deletions first, so a commit can never churn a line it added itself.
        let deletes = std::mem::take(&mut rows[i].deleted_fingerprints);
        for key in deletes {
            if let Some(q) = pool.get_mut(&key) {
                while let Some(&(ts, _)) = q.front() {
                    if now - ts > horizon_secs {
                        q.pop_front();
                    } else {
                        break;
                    }
                }
                if let Some((_, Adder::Row(idx))) = q.pop_front() {
                    rows[idx].counts.churned += 1;
                }
                if q.is_empty() {
                    pool.remove(&key);
                }
            }
        }
        let adds = std::mem::take(&mut rows[i].added_fingerprints);
        for key in adds {
            pool.entry(key).or_default().push_back((now, Adder::Row(i)));
        }
    }

    let mut still_open = Vec::new();
    for ((path, fp), q) in pool {
        for (ts, adder) in q {
            if newest.saturating_sub(ts) > horizon_secs {
                continue;
            }
            if let Adder::Row(idx) = adder {
                still_open.push(PendingAddition {
                    sha: rows[idx].sha.clone(),
                    path: path.clone(),
                    fingerprint: fp,
                    timestamp_unix: ts,
                });
            }
        }
    }
    still_open.sort_by(|a, b| {
        a.timestamp_unix
            .cmp(&b.timestamp_unix)
            .then_with(|| a.sha.cmp(&b.sha))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.fingerprint.cmp(&b.fingerprint))
    });
    still_open
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(sha: &str, ts: i64, adds: &[(&str, u64)], dels: &[(&str, u64)]) -> CommitSignals {
        CommitSignals {
            sha: sha.into(),
            timestamp_unix: ts,
            added_fingerprints: adds.iter().map(|(p, f)| (p.to_string(), *f)).collect(),
            deleted_fingerprints: dels.iter().map(|(p, f)| (p.to_string(), *f)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_line_removed_inside_the_horizon_churns_its_adder() {
        let day = 86_400;
        let mut rows = vec![
            row("b", 3 * day, &[], &[("a.rs", 1)]),
            row("a", 0, &[("a.rs", 1), ("a.rs", 2)], &[]),
            row("c", 40 * day, &[], &[("a.rs", 2)]),
        ];
        let open = attribute(&mut rows, &[], DEFAULT_HORIZON_SECS);
        let a = rows.iter().find(|r| r.sha == "a").unwrap();
        assert_eq!(a.counts.churned, 1);
        assert!(rows.iter().all(|r| r.added_fingerprints.is_empty()));
        assert!(
            open.is_empty(),
            "line 2 is older than the horizon by the end"
        );
    }

    #[test]
    fn same_commit_never_churns_itself_and_matching_is_one_to_one() {
        let mut rows = vec![
            row("a", 0, &[("a.rs", 7)], &[("a.rs", 7)]),
            row("b", 10, &[("a.rs", 7)], &[]),
            row("c", 20, &[], &[("a.rs", 7)]),
            row("d", 30, &[], &[("a.rs", 7)]),
        ];
        attribute(&mut rows, &[], 1_000);
        assert_eq!(rows[0].counts.churned, 1); // consumed by c
        assert_eq!(rows[1].counts.churned, 1); // consumed by d
    }

    #[test]
    fn pending_additions_carry_across_a_refresh() {
        // Pass 1: commit a adds a line; nothing deletes it yet.
        let mut rows = vec![row("a", 100, &[("a.rs", 9)], &[])];
        let open = attribute(&mut rows, &[], 1_000);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].sha, "a");
        // Pass 2: the baseline row `a` (fingerprints gone) plus a new commit deleting the line.
        let mut rows = vec![
            CommitSignals {
                sha: "a".into(),
                timestamp_unix: 100,
                ..Default::default()
            },
            row("b", 500, &[], &[("a.rs", 9)]),
        ];
        let open2 = attribute(&mut rows, &open, 1_000);
        assert_eq!(rows[0].counts.churned, 1);
        assert!(open2.is_empty());
    }
}
