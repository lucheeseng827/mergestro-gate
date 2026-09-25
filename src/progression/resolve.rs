// SPDX-License-Identifier: Apache-2.0
//! Resolve a [`ProgressionSpec`] against repository history into a
//! [`ProgressionSnapshot`] — the one artifact everything downstream reads.
//!
//! The snapshot is the machine contract: the SVG renders it, the README block
//! summarises it, the control-plane canvas draws it, and the ingest record
//! carries it. Layout (`col`/`row`) is computed **here**, not in the renderers,
//! so an operator comparing the README's SVG against the console sees the same
//! tree in the same arrangement rather than two drawings of one graph.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::history::{self, Commit};
use super::spec::{compile_glob, NodeSpec, PartSpec, ProgressionSpec};

/// How many example commits a part carries into the snapshot.
///
/// The snapshot is uploaded to the control plane, whose ingest body cap is
/// 1 MiB. Evidence is there to answer "what closed this?", and the three most
/// recent commits answer it; the full list would turn a 30-node tree into a
/// megabyte of shas nobody scrolls.
pub const MAX_EVIDENCE_PER_PART: usize = 3;

/// Where a milestone stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    /// A requirement is not done yet.
    Locked,
    /// Unblocked, nothing started.
    Available,
    /// Some evidence landed; not all parts closed.
    InProgress,
    /// Every part closed and every requirement done.
    Done,
}

impl NodeState {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeState::Locked => "locked",
            NodeState::Available => "available",
            NodeState::InProgress => "in_progress",
            NodeState::Done => "done",
        }
    }

    /// Human label for the SVG legend and the canvas.
    pub fn label(self) -> &'static str {
        match self {
            NodeState::Locked => "Locked",
            NodeState::Available => "Ready",
            NodeState::InProgress => "In progress",
            NodeState::Done => "Done",
        }
    }
}

/// One commit that closed (or is closing) a part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub sha: String,
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    pub timestamp_unix: u64,
}

/// A part, resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartProgress {
    pub id: String,
    pub title: String,
    pub done: bool,
    pub xp: u32,
    /// `true` when the spec closed it by hand rather than history closing it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub manual: bool,
    pub have_commits: u32,
    pub need_commits: u32,
    pub have_prs: u32,
    pub need_prs: u32,
    /// Up to [`MAX_EVIDENCE_PER_PART`] most recent matching commits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRef>,
}

/// A milestone, resolved and placed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeProgress {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    pub state: NodeState,
    pub xp_earned: u32,
    pub xp_total: u32,
    /// Closed parts over total, in basis points — integer so every consumer
    /// rounds the same way. A part-less marker node reads 10000 or 0.
    pub pct_bp: u32,
    pub parts: Vec<PartProgress>,
    /// Distinct commits matching any of this node's parts.
    pub commits: u32,
    /// Distinct PRs among them, ascending.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prs: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_activity_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_unix: Option<u64>,
    /// Layout column (0 = no requirements) and row within the column.
    pub col: u32,
    pub row: u32,
}

/// Tree-wide rollup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressionTotals {
    pub xp_earned: u32,
    pub xp_total: u32,
    pub pct_bp: u32,
    pub nodes_total: u32,
    pub nodes_done: u32,
    pub nodes_in_progress: u32,
    pub nodes_available: u32,
    pub nodes_locked: u32,
    pub parts_total: u32,
    pub parts_done: u32,
    /// Distinct commits that closed or advanced anything.
    pub commits: u32,
    /// Distinct PRs likewise.
    pub prs: u32,
    /// 1-based band the earned XP falls in.
    pub level: u32,
    /// Earned XP at which the current level started, and at which the next
    /// begins — the two numbers a progress bar needs and neither of which can
    /// be recovered from `level` alone once a spec sets custom bands.
    pub level_floor_xp: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_level_xp: Option<u32>,
}

/// The resolved tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressionSnapshot {
    pub version: u32,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season: Option<String>,
    /// When this snapshot was resolved. Deliberately absent from the SVG — a
    /// wall clock in a generated file makes every refresh a diff, and `--check`
    /// would then fail on a repository nobody changed.
    pub generated_at_unix: u64,
    /// The commit the tree was resolved at.
    pub head_sha: String,
    /// How far back the walk looked, when the spec bounded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_days: Option<u32>,
    pub totals: ProgressionTotals,
    pub nodes: Vec<NodeProgress>,
}

impl ProgressionSnapshot {
    /// Pretty JSON, newline-terminated — what `--json` writes and what a human
    /// diffing a generated file wants to read.
    pub fn render_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into());
        s.push('\n');
        s
    }

    pub fn node(&self, id: &str) -> Option<&NodeProgress> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Widest column index, i.e. how many layers the tree has minus one.
    pub fn columns(&self) -> u32 {
        self.nodes.iter().map(|n| n.col).max().map_or(0, |c| c + 1)
    }

    /// Tallest column, in rows.
    pub fn rows(&self) -> u32 {
        self.nodes.iter().map(|n| n.row).max().map_or(0, |r| r + 1)
    }
}

/// A part's compiled matcher, built once per part rather than per commit.
struct PartMatcher {
    paths: Vec<regex::Regex>,
    subject: Option<regex::Regex>,
    authors: Vec<String>,
}

impl PartMatcher {
    fn build(node: &NodeSpec, part: &PartSpec) -> anyhow::Result<Self> {
        // Part globs narrow the node's; with none of its own a part inherits the
        // node's scope, which is what makes a node-level `paths` worth writing.
        let globs = if part.evidence.paths.is_empty() {
            &node.paths
        } else {
            &part.evidence.paths
        };
        let paths = globs
            .iter()
            .map(|g| compile_glob(g))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let subject = match &part.evidence.subject {
            Some(p) => Some(
                regex::RegexBuilder::new(p)
                    .case_insensitive(true)
                    .build()
                    .map_err(|e| anyhow::anyhow!("bad `subject` regex {p:?}: {e}"))?,
            ),
            None => None,
        };
        Ok(PartMatcher {
            paths,
            subject,
            authors: part
                .evidence
                .authors
                .iter()
                .map(|a| a.to_lowercase())
                .collect(),
        })
    }

    /// Whether a commit counts toward this part.
    ///
    /// Selectors are ANDed: a rule with both `paths` and `subject` means "a
    /// commit in this area that also says this", which is how an author scopes
    /// "the docs for the Python lane" without matching every docs commit.
    fn matches(&self, c: &Commit) -> bool {
        if !self.authors.is_empty() {
            let a = c.author.to_lowercase();
            if !self.authors.iter().any(|want| a.contains(want)) {
                return false;
            }
        }
        if let Some(re) = &self.subject {
            if !re.is_match(&c.message) {
                return false;
            }
        }
        if !self.paths.is_empty() {
            if !c
                .paths
                .iter()
                .any(|p| self.paths.iter().any(|re| re.is_match(p)))
            {
                return false;
            }
        } else if self.subject.is_none() {
            // Nothing selects — a `manual`/threshold-only rule matches no commit
            // rather than every one. The opposite would close a `commits: 3`
            // rule on any three commits in the repository.
            return false;
        }
        true
    }
}

/// Resolve `spec` against `commits`, at `head_sha`, as of `now_unix`.
///
/// `commits` should be newest-first (what [`history::walk`] returns); the
/// ordering is what makes "the three most recent" evidence cheap.
pub fn resolve(
    spec: &ProgressionSpec,
    commits: &[Commit],
    head_sha: &str,
    now_unix: u64,
) -> anyhow::Result<ProgressionSnapshot> {
    let order = layer_order(spec);
    let mut done_by_id: HashMap<&str, bool> = HashMap::new();
    let mut resolved: HashMap<&str, NodeProgress> = HashMap::new();
    let mut all_commits: Vec<&str> = Vec::new();
    let mut all_prs: Vec<u64> = Vec::new();

    // Topological order: a node's `done` depends on its requirements', so the
    // requirements must already be resolved when we get here.
    for &idx in &order.topo {
        let node = &spec.nodes[idx];
        let mut parts = Vec::with_capacity(node.parts.len());
        let mut node_commits: Vec<&Commit> = Vec::new();

        for part in &node.parts {
            let matcher = PartMatcher::build(node, part)?;
            let matching: Vec<&Commit> = commits.iter().filter(|c| matcher.matches(c)).collect();
            let prs = history::distinct_prs(&matching);

            let need_commits = part.evidence.required_commits();
            let need_prs = part.evidence.required_prs();
            let have_commits = matching.len() as u32;
            let have_prs = prs.len() as u32;
            let done = part.evidence.manual
                || (have_commits >= need_commits
                    && have_prs >= need_prs
                    && (need_commits > 0 || need_prs > 0));

            node_commits.extend(matching.iter().copied());
            parts.push(PartProgress {
                id: part.id.clone(),
                title: part.title.clone(),
                done,
                xp: part.xp,
                manual: part.evidence.manual,
                have_commits,
                need_commits,
                have_prs,
                need_prs,
                evidence: matching
                    .iter()
                    .take(MAX_EVIDENCE_PER_PART)
                    .map(|c| EvidenceRef {
                        sha: c.sha.clone(),
                        subject: truncate(&c.subject, 120),
                        pr: c.pr,
                        timestamp_unix: c.timestamp_unix,
                    })
                    .collect(),
            });
        }

        node_commits.sort_by(|a, b| a.sha.cmp(&b.sha));
        node_commits.dedup_by(|a, b| a.sha == b.sha);
        let node_prs = history::distinct_prs(&node_commits);
        all_commits.extend(node_commits.iter().map(|c| c.sha.as_str()));
        all_prs.extend(node_prs.iter().copied());

        let xp_total: u32 = node.parts.iter().map(|p| p.xp).sum();
        let xp_earned: u32 = parts.iter().filter(|p| p.done).map(|p| p.xp).sum();
        let parts_closed = parts.iter().all(|p| p.done);
        let requirements_met = node
            .requires
            .iter()
            .all(|r| done_by_id.get(r.as_str()).copied().unwrap_or(false));
        let any_evidence = parts.iter().any(|p| p.done || p.have_commits > 0);

        let state = if parts_closed && requirements_met {
            NodeState::Done
        } else if any_evidence {
            NodeState::InProgress
        } else if requirements_met {
            NodeState::Available
        } else {
            NodeState::Locked
        };
        done_by_id.insert(node.id.as_str(), state == NodeState::Done);

        let pct_bp = if node.parts.is_empty() {
            if state == NodeState::Done {
                10_000
            } else {
                0
            }
        } else {
            let done = parts.iter().filter(|p| p.done).count() as u64;
            ((done * 10_000) / node.parts.len() as u64) as u32
        };

        resolved.insert(
            node.id.as_str(),
            NodeProgress {
                id: node.id.clone(),
                title: node.title.clone(),
                summary: node.summary.clone(),
                requires: node.requires.clone(),
                state,
                xp_earned,
                xp_total,
                pct_bp,
                parts,
                commits: node_commits.len() as u32,
                prs: node_prs,
                first_activity_unix: node_commits.iter().map(|c| c.timestamp_unix).min(),
                last_activity_unix: node_commits.iter().map(|c| c.timestamp_unix).max(),
                col: order.col[idx],
                row: order.row[idx],
            },
        );
    }

    // Emit in spec order, not topological order: the author's order is the one
    // they will look for in the JSON, and the canvas reads `col`/`row` anyway.
    let nodes: Vec<NodeProgress> = spec
        .nodes
        .iter()
        .filter_map(|n| resolved.remove(n.id.as_str()))
        .collect();

    all_commits.sort_unstable();
    all_commits.dedup();
    all_prs.sort_unstable();
    all_prs.dedup();

    let xp_total: u32 = nodes.iter().map(|n| n.xp_total).sum();
    let xp_earned: u32 = nodes.iter().map(|n| n.xp_earned).sum();
    let (level, level_floor_xp, next_level_xp) = level_for(&spec.level_thresholds(), xp_earned);

    let totals = ProgressionTotals {
        xp_earned,
        xp_total,
        pct_bp: if xp_total == 0 {
            0
        } else {
            ((xp_earned as u64 * 10_000) / xp_total as u64) as u32
        },
        nodes_total: nodes.len() as u32,
        nodes_done: count(&nodes, NodeState::Done),
        nodes_in_progress: count(&nodes, NodeState::InProgress),
        nodes_available: count(&nodes, NodeState::Available),
        nodes_locked: count(&nodes, NodeState::Locked),
        parts_total: nodes.iter().map(|n| n.parts.len() as u32).sum(),
        parts_done: nodes
            .iter()
            .map(|n| n.parts.iter().filter(|p| p.done).count() as u32)
            .sum(),
        commits: all_commits.len() as u32,
        prs: all_prs.len() as u32,
        level,
        level_floor_xp,
        next_level_xp,
    };

    Ok(ProgressionSnapshot {
        version: 1,
        title: spec.title.clone(),
        season: spec.season.clone(),
        generated_at_unix: now_unix,
        head_sha: head_sha.to_string(),
        since_days: spec.since_days,
        totals,
        nodes,
    })
}

fn count(nodes: &[NodeProgress], want: NodeState) -> u32 {
    nodes.iter().filter(|n| n.state == want).count() as u32
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The band `xp` falls in, plus the band's floor and the next band's floor.
pub fn level_for(thresholds: &[u32], xp: u32) -> (u32, u32, Option<u32>) {
    let mut level = 1u32;
    let mut floor = 0u32;
    for (i, &t) in thresholds.iter().enumerate() {
        if xp >= t {
            level = i as u32 + 1;
            floor = t;
        } else {
            return (level, floor, Some(t));
        }
    }
    (level, floor, None)
}

/// Column/row placement for every node, plus a topological visit order.
struct Order {
    col: Vec<u32>,
    row: Vec<u32>,
    topo: Vec<usize>,
}

/// Layered placement: column is the longest path from a root, so a node always
/// sits to the right of everything it needs. Rows are assigned per column by
/// the average row of a node's requirements (a one-pass barycenter), ties
/// broken by spec order — which keeps the layout stable when a spec grows, and
/// stable is what makes the generated SVG diffable.
fn layer_order(spec: &ProgressionSpec) -> Order {
    let n = spec.nodes.len();
    let index: HashMap<&str, usize> = spec
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.id.as_str(), i))
        .collect();

    let mut col = vec![0u32; n];
    // Longest-path layering by repeated relaxation. The spec is validated
    // acyclic before we get here, so `n` rounds is an upper bound that the loop
    // exits well short of; the bound only stops a hand-built `Order` in a test
    // from spinning.
    for _ in 0..n {
        let mut changed = false;
        for (i, node) in spec.nodes.iter().enumerate() {
            let want = node
                .requires
                .iter()
                .filter_map(|r| index.get(r.as_str()))
                .map(|&j| col[j] + 1)
                .max()
                .unwrap_or(0);
            if want > col[i] {
                col[i] = want;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut topo: Vec<usize> = (0..n).collect();
    topo.sort_by_key(|&i| (col[i], i));

    let mut row = vec![0u32; n];
    let max_col = col.iter().copied().max().unwrap_or(0);
    for c in 0..=max_col {
        let mut members: Vec<usize> = (0..n).filter(|&i| col[i] == c).collect();
        members.sort_by(|&a, &b| {
            let ka = barycenter(spec, &index, &row, &col, a);
            let kb = barycenter(spec, &index, &row, &col, b);
            ka.partial_cmp(&kb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.cmp(&b))
        });
        for (slot, &i) in members.iter().enumerate() {
            row[i] = slot as u32;
        }
    }

    Order { col, row, topo }
}

/// Average row of a node's already-placed requirements; `f64::MAX` sentinel
/// avoided by falling back to the spec index, so a root keeps its authored order.
fn barycenter(
    spec: &ProgressionSpec,
    index: &HashMap<&str, usize>,
    row: &[u32],
    col: &[u32],
    i: usize,
) -> f64 {
    let parents: Vec<usize> = spec.nodes[i]
        .requires
        .iter()
        .filter_map(|r| index.get(r.as_str()).copied())
        .filter(|&j| col[j] < col[i])
        .collect();
    if parents.is_empty() {
        return i as f64;
    }
    parents.iter().map(|&j| row[j] as f64).sum::<f64>() / parents.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progression::spec::{Evidence, NodeSpec, PartSpec};

    fn commit(sha: &str, subject: &str, paths: &[&str], ts: u64, pr: Option<u64>) -> Commit {
        Commit {
            sha: sha.into(),
            subject: subject.into(),
            message: subject.into(),
            author: "dev".into(),
            timestamp_unix: ts,
            paths: paths.iter().map(|p| p.to_string()).collect(),
            pr,
        }
    }

    fn part(id: &str, ev: Evidence) -> PartSpec {
        PartSpec {
            id: id.into(),
            title: id.into(),
            xp: 10,
            evidence: ev,
        }
    }

    fn node(id: &str, requires: &[&str], parts: Vec<PartSpec>) -> NodeSpec {
        NodeSpec {
            id: id.into(),
            title: id.into(),
            summary: None,
            requires: requires.iter().map(|s| s.to_string()).collect(),
            paths: Vec::new(),
            parts,
        }
    }

    #[test]
    fn a_part_with_no_paths_of_its_own_is_scoped_to_its_node() {
        // What makes a node-level `paths` worth writing, and what the worked example in
        // `examples/progression/specs/fresh-service.yaml` leans on: a part that adds only a
        // subject rule matches inside its node's globs, not across the whole repository. The
        // opposite would quietly close "three commits mentioning backpressure" on three
        // commits from an unrelated corner of the repo.
        let mut scoped = node(
            "ingest",
            &[],
            vec![part(
                "bp",
                Evidence {
                    subject: Some("backpressure".into()),
                    commits: Some(2),
                    ..Default::default()
                },
            )],
        );
        scoped.paths = vec!["src/ingest/**".into()];
        let s = spec(vec![scoped]);
        let commits = vec![
            commit("1", "feat: backpressure", &["src/ingest/queue.rs"], 3, None),
            commit("2", "feat: backpressure", &["src/ingest/pool.rs"], 2, None),
            commit(
                "3",
                "fix: backpressure in the UI",
                &["apps/console/x.js"],
                1,
                None,
            ),
        ];
        let snap = resolve(&s, &commits, "head", 0).unwrap();
        assert_eq!(
            snap.nodes[0].parts[0].have_commits, 2,
            "the third commit says the word but is outside the node's scope"
        );
        assert!(snap.nodes[0].parts[0].done);
    }

    fn spec(nodes: Vec<NodeSpec>) -> ProgressionSpec {
        ProgressionSpec {
            version: 1,
            title: "plan".into(),
            season: None,
            pr_pattern: crate::progression::spec::DEFAULT_PR_PATTERN.into(),
            levels: Vec::new(),
            since_days: None,
            nodes,
        }
    }

    #[test]
    fn a_part_closes_on_its_declared_threshold_and_not_before() {
        let s = spec(vec![node(
            "a",
            &[],
            vec![part(
                "p",
                Evidence {
                    paths: vec!["src/**".into()],
                    commits: Some(2),
                    ..Default::default()
                },
            )],
        )]);
        let one = vec![commit("1", "x", &["src/a.rs"], 10, None)];
        let snap = resolve(&s, &one, "head", 0).unwrap();
        assert_eq!(snap.nodes[0].state, NodeState::InProgress);
        assert_eq!(snap.totals.xp_earned, 0);

        let two = vec![
            commit("1", "x", &["src/a.rs"], 10, None),
            commit("2", "y", &["src/b.rs"], 20, None),
        ];
        let snap = resolve(&s, &two, "head", 0).unwrap();
        assert_eq!(snap.nodes[0].state, NodeState::Done);
        assert_eq!(snap.totals.xp_earned, 10);
        assert_eq!(snap.nodes[0].pct_bp, 10_000);
    }

    #[test]
    fn a_locked_node_stays_locked_until_its_requirement_is_done() {
        let s = spec(vec![
            node(
                "first",
                &[],
                vec![part(
                    "p",
                    Evidence {
                        paths: vec!["a/**".into()],
                        ..Default::default()
                    },
                )],
            ),
            node(
                "second",
                &["first"],
                vec![part(
                    "p",
                    Evidence {
                        paths: vec!["b/**".into()],
                        ..Default::default()
                    },
                )],
            ),
        ]);

        // Work landed in `b` first — the second node shows progress but cannot
        // be `done` while its requirement is open. A progression that let it
        // would report a plan completed out of order as completed in order.
        let out_of_order = vec![commit("1", "x", &["b/x.rs"], 10, None)];
        let snap = resolve(&s, &out_of_order, "head", 0).unwrap();
        assert_eq!(snap.node("first").unwrap().state, NodeState::Available);
        assert_eq!(snap.node("second").unwrap().state, NodeState::InProgress);

        let both = vec![
            commit("1", "x", &["b/x.rs"], 10, None),
            commit("2", "y", &["a/y.rs"], 20, None),
        ];
        let snap = resolve(&s, &both, "head", 0).unwrap();
        assert_eq!(snap.node("first").unwrap().state, NodeState::Done);
        assert_eq!(snap.node("second").unwrap().state, NodeState::Done);
    }

    #[test]
    fn nothing_selecting_closes_nothing() {
        // `commits: 3` with no selector must not be closed by three unrelated
        // commits — otherwise every threshold-only rule closes itself.
        let s = spec(vec![node(
            "a",
            &[],
            vec![part(
                "p",
                Evidence {
                    commits: Some(3),
                    ..Default::default()
                },
            )],
        )]);
        let cs = vec![
            commit("1", "x", &["z/1"], 1, None),
            commit("2", "x", &["z/2"], 2, None),
            commit("3", "x", &["z/3"], 3, None),
        ];
        let snap = resolve(&s, &cs, "head", 0).unwrap();
        assert!(!snap.nodes[0].parts[0].done);
        assert_eq!(snap.nodes[0].parts[0].have_commits, 0);
    }

    #[test]
    fn manual_closes_without_history_and_says_so() {
        let s = spec(vec![node(
            "a",
            &[],
            vec![part(
                "p",
                Evidence {
                    manual: true,
                    ..Default::default()
                },
            )],
        )]);
        let snap = resolve(&s, &[], "head", 0).unwrap();
        assert_eq!(snap.nodes[0].state, NodeState::Done);
        assert!(snap.nodes[0].parts[0].manual);
    }

    #[test]
    fn pr_thresholds_count_distinct_prs_not_commits() {
        let s = spec(vec![node(
            "a",
            &[],
            vec![part(
                "p",
                Evidence {
                    paths: vec!["src/**".into()],
                    prs: Some(2),
                    commits: Some(0),
                    ..Default::default()
                },
            )],
        )]);
        // Three commits, one PR: not enough.
        let one_pr = vec![
            commit("1", "a (#7)", &["src/a"], 1, Some(7)),
            commit("2", "b (#7)", &["src/b"], 2, Some(7)),
            commit("3", "c (#7)", &["src/c"], 3, Some(7)),
        ];
        let snap = resolve(&s, &one_pr, "head", 0).unwrap();
        assert_eq!(snap.nodes[0].parts[0].have_prs, 1);
        assert!(!snap.nodes[0].parts[0].done);

        let two_prs = vec![
            commit("1", "a (#7)", &["src/a"], 1, Some(7)),
            commit("2", "b (#9)", &["src/b"], 2, Some(9)),
        ];
        let snap = resolve(&s, &two_prs, "head", 0).unwrap();
        assert!(snap.nodes[0].parts[0].done);
        assert_eq!(snap.nodes[0].prs, vec![7, 9]);
    }

    #[test]
    fn subject_and_paths_are_anded_not_ored() {
        let s = spec(vec![node(
            "a",
            &[],
            vec![part(
                "p",
                Evidence {
                    paths: vec!["docs/**".into()],
                    subject: Some("python".into()),
                    ..Default::default()
                },
            )],
        )]);
        let wrong_area = vec![commit("1", "python lane", &["src/python.rs"], 1, None)];
        assert!(!resolve(&s, &wrong_area, "h", 0).unwrap().nodes[0].parts[0].done);

        let wrong_subject = vec![commit("1", "rust lane", &["docs/x.md"], 1, None)];
        assert!(!resolve(&s, &wrong_subject, "h", 0).unwrap().nodes[0].parts[0].done);

        let both = vec![commit("1", "Python lane docs", &["docs/x.md"], 1, None)];
        assert!(resolve(&s, &both, "h", 0).unwrap().nodes[0].parts[0].done);
    }

    #[test]
    fn layout_columns_follow_the_longest_path_and_are_stable() {
        let s = spec(vec![
            node(
                "root",
                &[],
                vec![part(
                    "p",
                    Evidence {
                        manual: true,
                        ..Default::default()
                    },
                )],
            ),
            node(
                "mid",
                &["root"],
                vec![part(
                    "p",
                    Evidence {
                        manual: true,
                        ..Default::default()
                    },
                )],
            ),
            node(
                "far",
                &["root", "mid"],
                vec![part(
                    "p",
                    Evidence {
                        manual: true,
                        ..Default::default()
                    },
                )],
            ),
        ]);
        let snap = resolve(&s, &[], "h", 0).unwrap();
        assert_eq!(snap.node("root").unwrap().col, 0);
        assert_eq!(snap.node("mid").unwrap().col, 1);
        // `far` needs both, so it sits past the longer of the two — not next to `mid`.
        assert_eq!(snap.node("far").unwrap().col, 2);
        assert_eq!(snap.columns(), 3);

        // Re-resolving the same spec must place it identically; the SVG is
        // committed, and an unstable layout is a diff on every CI run.
        let again = resolve(&s, &[], "h", 0).unwrap();
        assert_eq!(snap.nodes, again.nodes);
    }

    #[test]
    fn rows_are_unique_within_a_column() {
        let s = spec(vec![
            node(
                "a",
                &[],
                vec![part(
                    "p",
                    Evidence {
                        manual: true,
                        ..Default::default()
                    },
                )],
            ),
            node(
                "b",
                &[],
                vec![part(
                    "p",
                    Evidence {
                        manual: true,
                        ..Default::default()
                    },
                )],
            ),
            node(
                "c",
                &[],
                vec![part(
                    "p",
                    Evidence {
                        manual: true,
                        ..Default::default()
                    },
                )],
            ),
        ]);
        let snap = resolve(&s, &[], "h", 0).unwrap();
        let mut rows: Vec<u32> = snap.nodes.iter().map(|n| n.row).collect();
        rows.sort_unstable();
        assert_eq!(rows, vec![0, 1, 2]);
        assert_eq!(snap.rows(), 3);
    }

    #[test]
    fn levels_report_the_band_floor_and_the_next_band() {
        let bands = [0u32, 50, 150];
        assert_eq!(level_for(&bands, 0), (1, 0, Some(50)));
        assert_eq!(level_for(&bands, 49), (1, 0, Some(50)));
        assert_eq!(level_for(&bands, 50), (2, 50, Some(150)));
        // Past the last band there is no next one — the bar is full, not divided by zero.
        assert_eq!(level_for(&bands, 9_000), (3, 150, None));
    }

    #[test]
    fn evidence_is_capped_but_totals_are_not() {
        let s = spec(vec![node(
            "a",
            &[],
            vec![part(
                "p",
                Evidence {
                    paths: vec!["src/**".into()],
                    ..Default::default()
                },
            )],
        )]);
        let many: Vec<Commit> = (0..10)
            .map(|i| commit(&format!("{i:040}"), "x", &["src/a"], i, None))
            .collect();
        let snap = resolve(&s, &many, "h", 0).unwrap();
        assert_eq!(snap.nodes[0].parts[0].evidence.len(), MAX_EVIDENCE_PER_PART);
        assert_eq!(snap.nodes[0].parts[0].have_commits, 10);
        assert_eq!(snap.nodes[0].commits, 10);
    }
}
