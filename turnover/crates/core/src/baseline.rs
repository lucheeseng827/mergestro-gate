//! The baseline file: one classified row per commit, append-only. It is the product's
//! precondition — the gate has nothing to compare against until it exists — and the reason
//! the walker is parallel. Rows are keyed by sha, so a refresh walks only unseen commits and
//! merges; a full re-walk is only needed when the classifier config changes.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::churn::{PendingAddition, DEFAULT_HORIZON_SECS};
use crate::signals::{CommitSignals, SignalConfig};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The tip the newest walk started from — the next refresh hides everything below it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    pub generated_at_unix: i64,
    #[serde(default)]
    pub config: SignalConfig,
    #[serde(default)]
    pub commits: Vec<CommitSignals>,
    /// Churn horizon the rows were attributed with.
    #[serde(default = "default_horizon")]
    pub churn_horizon_secs: i64,
    /// Additions still inside the churn horizon at the newest row — see [`crate::churn`].
    /// Stored grouped per commit and path (one sha and one path string per group instead of
    /// per line), which is what keeps a two-week tail of a busy repository to a few hundred
    /// kilobytes rather than several megabytes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<PendingGroup>,
}

/// The on-disk shape of pending additions: one commit, one path, many fingerprints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingGroup {
    pub sha: String,
    pub timestamp_unix: i64,
    pub path: String,
    pub fingerprints: Vec<u64>,
}

/// Group flat pending additions by `(sha, timestamp, path)` for storage.
pub fn group_pending(flat: Vec<PendingAddition>) -> Vec<PendingGroup> {
    let mut map: std::collections::BTreeMap<(String, i64, String), Vec<u64>> =
        std::collections::BTreeMap::new();
    for p in flat {
        map.entry((p.sha, p.timestamp_unix, p.path))
            .or_default()
            .push(p.fingerprint);
    }
    map.into_iter()
        .map(|((sha, timestamp_unix, path), fingerprints)| PendingGroup {
            sha,
            timestamp_unix,
            path,
            fingerprints,
        })
        .collect()
}

/// Expand stored groups back into the flat form [`crate::churn::attribute`] consumes.
pub fn ungroup_pending(groups: &[PendingGroup]) -> Vec<PendingAddition> {
    groups
        .iter()
        .flat_map(|g| {
            g.fingerprints
                .iter()
                .map(move |&fingerprint| PendingAddition {
                    sha: g.sha.clone(),
                    path: g.path.clone(),
                    fingerprint,
                    timestamp_unix: g.timestamp_unix,
                })
        })
        .collect()
}

fn default_horizon() -> i64 {
    DEFAULT_HORIZON_SECS
}

#[derive(Debug, thiserror::Error)]
pub enum BaselineError {
    #[error("baseline is format version {found}, this build reads version {expected}")]
    Version { found: u32, expected: u32 },
    #[error("baseline JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl Baseline {
    pub fn new(config: SignalConfig, generated_at_unix: i64) -> Baseline {
        Baseline {
            version: FORMAT_VERSION,
            repo: None,
            head: None,
            generated_at_unix,
            config,
            commits: Vec::new(),
            churn_horizon_secs: DEFAULT_HORIZON_SECS,
            pending: Vec::new(),
        }
    }

    pub fn from_json(text: &str) -> Result<Baseline, BaselineError> {
        let b: Baseline = serde_json::from_str(text)?;
        if b.version != FORMAT_VERSION {
            return Err(BaselineError::Version {
                found: b.version,
                expected: FORMAT_VERSION,
            });
        }
        Ok(b)
    }

    pub fn to_json(&self) -> Result<String, BaselineError> {
        Ok(serde_json::to_string(self)?)
    }

    pub fn known_shas(&self) -> HashSet<&str> {
        self.commits.iter().map(|c| c.sha.as_str()).collect()
    }

    /// Add rows not already present (by sha) and keep the file sorted by time, oldest first.
    /// Returns how many rows were new.
    pub fn merge(&mut self, rows: Vec<CommitSignals>) -> usize {
        let mut known: HashSet<String> = self.commits.iter().map(|c| c.sha.clone()).collect();
        let mut added = 0;
        for r in rows {
            // `insert` returns false for a sha already known — from the file or earlier in
            // this same batch, so a duplicated row never lands twice.
            if known.insert(r.sha.clone()) {
                self.commits.push(r);
                added += 1;
            }
        }
        self.commits.sort_by(|a, b| {
            a.timestamp_unix
                .cmp(&b.timestamp_unix)
                .then_with(|| a.sha.cmp(&b.sha))
        });
        added
    }

    /// `true` if the classifier settings differ from the ones the rows were produced with —
    /// mixing them would make every ratio incomparable with the old rows.
    pub fn config_differs(&self, cfg: &SignalConfig) -> bool {
        self.config.min_line_chars != cfg.min_line_chars
            || self.config.block_min_lines != cfg.block_min_lines
            || self.config.ignore_imports != cfg.ignore_imports
            || self.config.clone_type != cfg.clone_type
            || self.config.block_min_tokens != cfg.block_min_tokens
    }

    /// The pending additions in the flat form churn attribution consumes.
    pub fn pending_additions(&self) -> Vec<PendingAddition> {
        ungroup_pending(&self.pending)
    }

    /// Replace the pending additions (stored grouped).
    pub fn set_pending_additions(&mut self, flat: Vec<PendingAddition>) {
        self.pending = group_pending(flat);
    }

    pub fn newest_timestamp(&self) -> Option<i64> {
        self.commits.iter().map(|c| c.timestamp_unix).max()
    }

    pub fn oldest_timestamp(&self) -> Option<i64> {
        self.commits.iter().map(|c| c.timestamp_unix).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_merge_dedupe() {
        let mut b = Baseline::new(SignalConfig::default(), 1);
        let n = b.merge(vec![
            CommitSignals {
                sha: "b".into(),
                timestamp_unix: 2,
                ..Default::default()
            },
            CommitSignals {
                sha: "a".into(),
                timestamp_unix: 1,
                ..Default::default()
            },
        ]);
        assert_eq!(n, 2);
        assert_eq!(
            b.merge(vec![CommitSignals {
                sha: "a".into(),
                ..Default::default()
            }]),
            0
        );
        assert_eq!(b.commits[0].sha, "a");
        let text = b.to_json().unwrap();
        let back = Baseline::from_json(&text).unwrap();
        assert_eq!(back.commits.len(), 2);
        assert!(!back.config_differs(&SignalConfig::default()));
    }

    #[test]
    fn pending_additions_roundtrip_through_groups() {
        let mut b = Baseline::new(SignalConfig::default(), 1);
        let flat = vec![
            PendingAddition {
                sha: "a".into(),
                path: "x.rs".into(),
                fingerprint: 2,
                timestamp_unix: 5,
            },
            PendingAddition {
                sha: "a".into(),
                path: "x.rs".into(),
                fingerprint: 1,
                timestamp_unix: 5,
            },
            PendingAddition {
                sha: "b".into(),
                path: "y.rs".into(),
                fingerprint: 9,
                timestamp_unix: 6,
            },
        ];
        b.set_pending_additions(flat.clone());
        assert_eq!(b.pending.len(), 2);
        let back = Baseline::from_json(&b.to_json().unwrap()).unwrap();
        let mut got = back.pending_additions();
        got.sort_by_key(|p| (p.sha.clone(), p.fingerprint));
        let mut want = flat;
        want.sort_by_key(|p| (p.sha.clone(), p.fingerprint));
        assert_eq!(got, want);
    }

    #[test]
    fn wrong_version_is_refused() {
        let text = r#"{"version":99,"generated_at_unix":0,"commits":[]}"#;
        assert!(matches!(
            Baseline::from_json(text),
            Err(BaselineError::Version { found: 99, .. })
        ));
    }
}
