//! Aggregation over time ranges. Ratios are the unit of comparison everywhere: a window
//! against the baseline, a repo against the fleet, this quarter against last.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::attribution::Origin;
use crate::signals::{CommitSignals, Counts};

/// The four published ratios, each over significant added lines. `None` when the window
/// added nothing — a ratio over zero is not "zero", it is "no evidence".
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Ratios {
    pub copy_paste: Option<f64>,
    pub dup_block: Option<f64>,
    pub refactor: Option<f64>,
    pub churn: Option<f64>,
}

impl Ratios {
    pub fn of(c: &Counts) -> Ratios {
        let r = |n: u64| {
            if c.added == 0 {
                None
            } else {
                Some(n as f64 / c.added as f64)
            }
        };
        Ratios {
            copy_paste: r(c.copy_pasted),
            dup_block: r(c.dup_block),
            refactor: r(c.moved),
            churn: r(c.churned),
        }
    }
}

/// Per-language slice of an aggregate.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LanguageAggregate {
    pub counts: Counts,
    pub ratios: Ratios,
}

/// The slice of an aggregate that came from one origin (AI-coauthored or human).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OriginAggregate {
    pub commits: u64,
    pub counts: Counts,
    pub ratios: Ratios,
}

/// Totals and ratios over a set of commits.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    pub from_unix: i64,
    pub to_unix: i64,
    pub commits: u64,
    pub authors: u64,
    pub counts: Counts,
    pub ratios: Ratios,
    #[serde(default)]
    pub by_language: BTreeMap<String, LanguageAggregate>,
    /// `"ai"` and `"human"` slices — the split behind "is AI making this worse?".
    #[serde(default)]
    pub by_origin: BTreeMap<String, OriginAggregate>,
}

impl Aggregate {
    /// The AI-coauthored slice, if any commit in the range was.
    pub fn ai(&self) -> Option<&OriginAggregate> {
        self.by_origin.get(Origin::Ai.as_str())
    }

    /// The human slice, if any commit in the range was.
    pub fn human(&self) -> Option<&OriginAggregate> {
        self.by_origin.get(Origin::Human.as_str())
    }

    /// Share of significant added lines that came from AI-coauthored commits.
    pub fn ai_share(&self) -> Option<f64> {
        if self.counts.added == 0 {
            return None;
        }
        Some(self.ai().map_or(0.0, |a| a.counts.added as f64) / self.counts.added as f64)
    }
}

/// Aggregate rows with `from_unix <= timestamp < to_unix`. Merge commits are skipped by the
/// walker's default; if present they contribute like any row.
pub fn aggregate<'a>(
    rows: impl IntoIterator<Item = &'a CommitSignals>,
    from_unix: i64,
    to_unix: i64,
) -> Aggregate {
    let mut agg = Aggregate {
        from_unix,
        to_unix,
        ..Default::default()
    };
    let mut authors: BTreeSet<&str> = BTreeSet::new();
    for r in rows {
        if r.timestamp_unix < from_unix || r.timestamp_unix >= to_unix {
            continue;
        }
        agg.commits += 1;
        authors.insert(r.author.as_str());
        agg.counts.add(&r.counts);
        for (lang, c) in &r.by_language {
            agg.by_language
                .entry(lang.clone())
                .or_default()
                .counts
                .add(c);
        }
        let o = agg
            .by_origin
            .entry(r.origin.as_str().to_string())
            .or_default();
        o.commits += 1;
        o.counts.add(&r.counts);
    }
    agg.authors = authors.len() as u64;
    agg.ratios = Ratios::of(&agg.counts);
    for la in agg.by_language.values_mut() {
        la.ratios = Ratios::of(&la.counts);
    }
    for oa in agg.by_origin.values_mut() {
        oa.ratios = Ratios::of(&oa.counts);
    }
    agg
}

/// Consecutive buckets of `bucket_secs` from `from_unix` up to `to_unix` — the trend series.
pub fn series(
    rows: &[CommitSignals],
    from_unix: i64,
    to_unix: i64,
    bucket_secs: i64,
) -> Vec<Aggregate> {
    let bucket = bucket_secs.max(1);
    let mut out = Vec::new();
    let mut start = from_unix;
    while start < to_unix {
        let end = (start + bucket).min(to_unix);
        out.push(aggregate(rows, start, end));
        start = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ts: i64, author: &str, added: u64, pasted: u64) -> CommitSignals {
        let mut by_language = BTreeMap::new();
        by_language.insert(
            "rust".to_string(),
            Counts {
                added,
                copy_pasted: pasted,
                ..Default::default()
            },
        );
        CommitSignals {
            sha: format!("{ts}"),
            timestamp_unix: ts,
            author: author.into(),
            counts: Counts {
                added,
                copy_pasted: pasted,
                ..Default::default()
            },
            by_language,
            ..Default::default()
        }
    }

    #[test]
    fn aggregate_respects_the_half_open_range_and_computes_ratios() {
        let rows = vec![
            row(0, "a", 10, 1),
            row(5, "b", 10, 3),
            row(10, "a", 100, 100),
        ];
        let agg = aggregate(&rows, 0, 10);
        assert_eq!(agg.commits, 2);
        assert_eq!(agg.authors, 2);
        assert_eq!(agg.counts.added, 20);
        assert_eq!(agg.ratios.copy_paste, Some(0.2));
        assert_eq!(agg.ratios.churn, Some(0.0));
        assert_eq!(agg.by_language["rust"].counts.added, 20);
        assert_eq!(aggregate(&rows, 20, 30).ratios.copy_paste, None);
    }

    #[test]
    fn origins_split_the_aggregate() {
        let mut ai = row(1, "a", 100, 40);
        ai.origin = Origin::Ai;
        let rows = vec![ai, row(2, "b", 300, 30)];
        let agg = aggregate(&rows, 0, 10);
        assert_eq!(agg.ai().unwrap().commits, 1);
        assert_eq!(agg.ai().unwrap().ratios.copy_paste, Some(0.4));
        assert_eq!(agg.human().unwrap().ratios.copy_paste, Some(0.1));
        assert_eq!(agg.ai_share(), Some(0.25));
        assert_eq!(aggregate(&rows, 2, 3).ai(), None);
    }

    #[test]
    fn series_buckets_cover_the_range() {
        let rows = vec![row(1, "a", 1, 0), row(11, "a", 1, 0)];
        let s = series(&rows, 0, 20, 10);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].commits, 1);
        assert_eq!(s[1].commits, 1);
        assert_eq!(s[1].from_unix, 10);
    }
}
