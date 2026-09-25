// SPDX-License-Identifier: Apache-2.0
//! The progression **spec** — the hand-authored half of the tree.
//!
//! A spec is a plan someone wrote before the work happened: a sprint, a
//! quarter, a phased build. It is deliberately heuristic. Nothing here is
//! derived from the repository; the repository only ever *closes* what this
//! file declared. That split is the whole point — a tree mined purely from
//! history can describe what was done but never what was meant, and a plan
//! with no evidence rule attached is a wiki page that goes stale in a week.
//!
//! The shape:
//!
//! ```yaml
//! version: 1
//! title: Mergestro — phased build
//! nodes:
//!   - id: poc
//!     title: PoC — prove the catch
//!     parts:
//!       - id: diff
//!         title: Pure-Rust diff against the base ref
//!         evidence: { paths: ["src/diff.rs"], commits: 1 }
//!   - id: rollout
//!     requires: [poc]
//!     ...
//! ```
//!
//! `requires` makes the node set a DAG, not a literal tree: a milestone
//! routinely needs two predecessors, and forcing one parent would make the
//! author pick a lie. "Tree" is what it reads as on the canvas.

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The default XP a part is worth when it does not say.
pub const DEFAULT_PART_XP: u32 = 10;

/// Default commit-subject pattern for "which PR merged this": GitHub's squash
/// subject, `… (#1234)`. Overridable because GitLab writes `See merge request
/// !123` and a rebase-merge repo may write nothing at all.
pub const DEFAULT_PR_PATTERN: &str = r"\(#(\d+)\)";

/// Level thresholds (cumulative earned XP) when the spec does not set them.
/// Eight bands, widening — the first is reachable in a day so a new tree is not
/// stuck at level 1 for a quarter.
pub const DEFAULT_LEVELS: &[u32] = &[0, 50, 150, 350, 700, 1200, 2000, 3200];

/// A whole progression plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressionSpec {
    /// Spec format version. Only `1` is understood.
    #[serde(default = "one")]
    pub version: u32,
    /// Shown as the canvas / SVG heading.
    pub title: String,
    /// Free-form period label — "2026 H1", "Sprint 14". Rendered next to the title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season: Option<String>,
    /// Regex with one capture group: the PR number inside a commit subject.
    #[serde(default = "default_pr_pattern")]
    pub pr_pattern: String,
    /// Cumulative earned-XP thresholds, ascending, index 0 being level 1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<u32>,
    /// Ignore commits older than this many days. `None` walks the whole history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_days: Option<u32>,
    /// The milestones.
    pub nodes: Vec<NodeSpec>,
}

fn one() -> u32 {
    1
}

fn default_pr_pattern() -> String {
    DEFAULT_PR_PATTERN.to_string()
}

/// One milestone: a component, a phase, a feature area.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSpec {
    /// Stable identifier. Used by `requires`, by the SVG anchors and as the
    /// canvas selection key, so renaming one is a breaking change to any link.
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Milestones that must be `done` before this one leaves `locked`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Globs scoping *every* part of this node, unless the part narrows further.
    /// A node-level `paths` is the common case: the parts of "the Python engine"
    /// all live under the same directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// The breakdown. A node with no parts is a marker: it is `done` as soon as
    /// its requirements are, which is how a spec expresses "and then we shipped".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<PartSpec>,
}

/// One checkable piece of a milestone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartSpec {
    pub id: String,
    pub title: String,
    /// XP awarded when this part closes.
    #[serde(default = "default_part_xp")]
    pub xp: u32,
    /// What closes it. Every declared threshold must be met.
    #[serde(default)]
    pub evidence: Evidence,
}

fn default_part_xp() -> u32 {
    DEFAULT_PART_XP
}

/// The rule that turns repository history into a closed part.
///
/// All declared conditions are ANDed. `paths` and `subject` *select* commits;
/// `commits` and `prs` are thresholds over the selection. An empty
/// [`Evidence`] never closes on its own — that is a plan item nobody wired up,
/// and reporting it as done would be the failure mode this whole file exists to
/// avoid.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Evidence {
    /// Repo-relative globs (`**` crosses directories, `*` does not).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Regex matched case-insensitively against the commit subject + body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Commit authors (substring, case-insensitive) that count. Empty: anyone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    /// How many matching commits close it. Defaults to 1 when a selector is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits: Option<u32>,
    /// How many *distinct* PRs among the matching commits close it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prs: Option<u32>,
    /// Closed by hand. The escape hatch for work no commit can prove — a design
    /// review held, a vendor signed. Honest precisely because it is explicit and
    /// lives in the diff.
    #[serde(default, skip_serializing_if = "is_false")]
    pub manual: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Evidence {
    /// Whether any selector or manual flag is set. An evidence block that
    /// selects nothing is a spec error, not a part that is permanently open.
    pub fn is_wired(&self) -> bool {
        self.manual
            || !self.paths.is_empty()
            || self.subject.is_some()
            || self.commits.is_some()
            || self.prs.is_some()
    }

    /// The commit threshold actually applied: what was asked for, or 1 when a
    /// selector is present and no count was given.
    pub fn required_commits(&self) -> u32 {
        match self.commits {
            Some(n) => n,
            None if self.prs.is_some() => 0,
            None if self.paths.is_empty() && self.subject.is_none() => 0,
            None => 1,
        }
    }

    /// The distinct-PR threshold actually applied.
    pub fn required_prs(&self) -> u32 {
        self.prs.unwrap_or(0)
    }
}

impl ProgressionSpec {
    /// Parse and validate a spec from YAML on disk.
    pub fn from_yaml_file(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading progression spec {}", path.display()))?;
        let spec: ProgressionSpec = serde_yaml::from_str(&text)
            .with_context(|| format!("parsing progression spec {}", path.display()))?;
        spec.validate()
            .with_context(|| format!("validating progression spec {}", path.display()))?;
        Ok(spec)
    }

    /// The level bands in effect.
    pub fn level_thresholds(&self) -> Vec<u32> {
        if self.levels.is_empty() {
            DEFAULT_LEVELS.to_vec()
        } else {
            self.levels.clone()
        }
    }

    /// Reject the specs that would silently render wrong: duplicate ids,
    /// dangling `requires`, cycles, and parts nothing can ever close.
    ///
    /// A cycle is the one that matters most. Layering is `1 + max(layer of
    /// requirements)`, so a cycle is not a strange-looking canvas — it is a
    /// layout that does not terminate.
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!(
                "unsupported spec version {} (this build understands version 1)",
                self.version
            );
        }
        if self.title.trim().is_empty() {
            bail!("`title` is empty");
        }
        if self.nodes.is_empty() {
            bail!("`nodes` is empty — a progression tree needs at least one milestone");
        }
        regex::Regex::new(&self.pr_pattern)
            .with_context(|| format!("`pr_pattern` {:?} is not a valid regex", self.pr_pattern))?;
        if self.levels.windows(2).any(|w| w[0] >= w[1]) {
            bail!("`levels` must be strictly ascending");
        }

        let mut seen: HashSet<&str> = HashSet::new();
        for node in &self.nodes {
            if node.id.trim().is_empty() {
                bail!("a node has an empty `id`");
            }
            if !seen.insert(node.id.as_str()) {
                bail!("duplicate node id `{}`", node.id);
            }
            let mut part_ids: HashSet<&str> = HashSet::new();
            for part in &node.parts {
                if part.id.trim().is_empty() {
                    bail!("node `{}` has a part with an empty `id`", node.id);
                }
                if !part_ids.insert(part.id.as_str()) {
                    bail!("node `{}` has duplicate part id `{}`", node.id, part.id);
                }
                if !part.evidence.is_wired() {
                    bail!(
                        "node `{}` part `{}` declares no evidence — give it `paths`, `subject`, \
                         `commits`, `prs`, or `manual: true`",
                        node.id,
                        part.id
                    );
                }
                // A part whose every threshold is zero is wired but unclosable: the resolver
                // requires at least one threshold above zero before it will call a part done, so
                // this would sit open against any history forever. `commits: 0` is legitimate
                // ONLY as "count PRs, not commits" — paired with `prs`, as the example spec does
                // — and deleting that one `prs` line is how an author lands here by accident.
                if !part.evidence.manual
                    && part.evidence.required_commits() == 0
                    && part.evidence.required_prs() == 0
                {
                    bail!(
                        "node `{}` part `{}` sets every threshold to 0, so nothing can ever close \
                         it — raise `commits` or `prs`, or set `manual: true`",
                        node.id,
                        part.id
                    );
                }
                if let Some(pattern) = &part.evidence.subject {
                    regex::Regex::new(pattern).with_context(|| {
                        format!(
                            "node `{}` part `{}`: `subject` {pattern:?} is not a valid regex",
                            node.id, part.id
                        )
                    })?;
                }
                for glob in part.evidence.paths.iter().chain(node.paths.iter()) {
                    compile_glob(glob).with_context(|| {
                        format!("node `{}` part `{}`: bad path glob", node.id, part.id)
                    })?;
                }
            }
        }
        for node in &self.nodes {
            for req in &node.requires {
                if !seen.contains(req.as_str()) {
                    bail!("node `{}` requires unknown node `{}`", node.id, req);
                }
                if req == &node.id {
                    bail!("node `{}` requires itself", node.id);
                }
            }
        }
        self.reject_cycles()
    }

    /// Iterative three-colour DFS. Iterative rather than recursive because the
    /// spec is user input and a deep chain should be a rejected spec, not a
    /// blown stack.
    fn reject_cycles(&self) -> Result<()> {
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Open,
            Done,
        }
        let by_id: HashMap<&str, &NodeSpec> =
            self.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let mut mark: HashMap<&str, Mark> = HashMap::new();

        for root in &self.nodes {
            if mark.get(root.id.as_str()) == Some(&Mark::Done) {
                continue;
            }
            // (node, whether its requirements have already been pushed)
            let mut stack: Vec<(&str, bool)> = vec![(root.id.as_str(), false)];
            while let Some((id, expanded)) = stack.pop() {
                if expanded {
                    mark.insert(id, Mark::Done);
                    continue;
                }
                match mark.get(id) {
                    Some(Mark::Done) => continue,
                    Some(Mark::Open) => bail!("node `{id}` is part of a `requires` cycle"),
                    None => {}
                }
                mark.insert(id, Mark::Open);
                stack.push((id, true));
                if let Some(node) = by_id.get(id) {
                    for req in &node.requires {
                        if mark.get(req.as_str()) == Some(&Mark::Open) {
                            bail!("`requires` cycle through `{}` and `{}`", id, req);
                        }
                        stack.push((req.as_str(), false));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Translate one glob into an anchored regex.
///
/// Rules, chosen to match what a `.gitignore`-literate author expects:
/// `**/` matches zero or more leading directories, a bare `**` matches
/// anything, `*` stops at `/`, `?` is one non-`/` character. A pattern with no
/// wildcard and no extension boundary also matches everything *under* it, so
/// `src/diff.rs` matches the file and `docs` matches the directory's contents —
/// otherwise every author's first spec silently matches nothing.
pub fn compile_glob(glob: &str) -> Result<regex::Regex> {
    let g = glob.trim().trim_start_matches("./");
    if g.is_empty() {
        bail!("empty path glob");
    }
    let mut re = String::with_capacity(g.len() * 2 + 8);
    re.push('^');
    let bytes: Vec<char> = g.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            '*' if i + 1 < bytes.len() && bytes[i + 1] == '*' => {
                if i + 2 < bytes.len() && bytes[i + 2] == '/' {
                    re.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    re.push_str(".*");
                    i += 2;
                }
            }
            '*' => {
                re.push_str("[^/]*");
                i += 1;
            }
            '?' => {
                re.push_str("[^/]");
                i += 1;
            }
            c => {
                re.push_str(&regex::escape(&c.to_string()));
                i += 1;
            }
        }
    }
    // A plain prefix also covers everything beneath it.
    if !g.contains('*') && !g.contains('?') {
        re.push_str("(?:/.*)?");
    }
    re.push('$');
    regex::Regex::new(&re).with_context(|| format!("compiling glob {glob:?} (as {re:?})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every worked example under `examples/progression/specs/` loads and validates.
    ///
    /// They are the first thing a reader copies, and a rule that drifted — a renamed field, a
    /// tightened check — turns them into a paste that fails on someone else's first run. The
    /// tool has no other reason to read that directory, so nothing else would notice.
    #[test]
    fn the_worked_examples_still_load() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/progression/specs");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("examples/progression/specs is missing") {
            let path = entry.expect("reading the examples directory").path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let spec = ProgressionSpec::from_yaml_file(&path)
                .unwrap_or_else(|e| panic!("{} does not load: {e:#}", path.display()));
            assert!(
                spec.nodes.len() >= 3,
                "{} is too small to be worth copying",
                path.display()
            );
            checked += 1;
        }
        assert!(
            checked >= 3,
            "expected the three worked examples, found {checked}"
        );
    }

    fn spec_with(nodes: Vec<NodeSpec>) -> ProgressionSpec {
        ProgressionSpec {
            version: 1,
            title: "t".into(),
            season: None,
            pr_pattern: default_pr_pattern(),
            levels: Vec::new(),
            since_days: None,
            nodes,
        }
    }

    fn node(id: &str, requires: &[&str]) -> NodeSpec {
        NodeSpec {
            id: id.into(),
            title: id.into(),
            summary: None,
            requires: requires.iter().map(|s| s.to_string()).collect(),
            paths: Vec::new(),
            parts: vec![PartSpec {
                id: "p".into(),
                title: "p".into(),
                xp: 10,
                evidence: Evidence {
                    paths: vec!["src/**".into()],
                    ..Default::default()
                },
            }],
        }
    }

    #[test]
    fn globs_match_the_way_an_author_expects() {
        let g = compile_glob("src/**/*.rs").unwrap();
        assert!(g.is_match("src/a.rs"));
        assert!(g.is_match("src/deep/nested/a.rs"));
        assert!(!g.is_match("tests/a.rs"));

        let star = compile_glob("src/*.rs").unwrap();
        assert!(star.is_match("src/a.rs"));
        assert!(!star.is_match("src/deep/a.rs"));

        // A wildcard-free prefix covers the subtree, so `docs` is not a dead rule.
        let prefix = compile_glob("docs").unwrap();
        assert!(prefix.is_match("docs"));
        assert!(prefix.is_match("docs/CONFIG.md"));
        assert!(!prefix.is_match("docsite/x"));

        // Regex metacharacters in a path are literal.
        let dotted = compile_glob("a.b/c").unwrap();
        assert!(dotted.is_match("a.b/c"));
        assert!(!dotted.is_match("axb/c"));
    }

    #[test]
    fn cycles_are_rejected_rather_than_laid_out_forever() {
        let mut s = spec_with(vec![node("a", &["b"]), node("b", &["a"])]);
        let err = s.validate().unwrap_err().to_string();
        assert!(err.contains("cycle"), "{err}");

        // Self-reference is the degenerate cycle and gets its own message.
        s = spec_with(vec![node("a", &["a"])]);
        assert!(s.validate().unwrap_err().to_string().contains("itself"));

        // A diamond is a DAG, not a cycle: both sides must pass.
        s = spec_with(vec![
            node("root", &[]),
            node("l", &["root"]),
            node("r", &["root"]),
            node("join", &["l", "r"]),
        ]);
        s.validate().unwrap();
    }

    #[test]
    fn a_part_nothing_can_close_is_a_spec_error() {
        let mut s = spec_with(vec![node("a", &[])]);
        s.nodes[0].parts[0].evidence = Evidence::default();
        let err = s.validate().unwrap_err().to_string();
        assert!(err.contains("declares no evidence"), "{err}");
    }

    #[test]
    fn a_part_whose_every_threshold_is_zero_is_rejected_not_left_open_forever() {
        // `commits: 0` is legitimate paired with `prs` ("count PRs, not commits"). Alone it is a
        // part the resolver can never close, and `is_wired` does not catch it because `commits`
        // IS set — a plan with a milestone that never completes and a level that never rises.
        let mut s = spec_with(vec![node("a", &[])]);
        s.nodes[0].parts[0].evidence = Evidence {
            paths: vec!["src/**".into()],
            commits: Some(0),
            ..Default::default()
        };
        let err = s.validate().unwrap_err().to_string();
        assert!(err.contains("every threshold to 0"), "{err}");

        // Paired with a PR threshold it is exactly what the example spec does, and it passes.
        s.nodes[0].parts[0].evidence.prs = Some(2);
        s.validate().unwrap();

        // `manual` closes without thresholds and is unaffected.
        s.nodes[0].parts[0].evidence = Evidence {
            manual: true,
            commits: Some(0),
            ..Default::default()
        };
        s.validate().unwrap();
    }

    #[test]
    fn duplicate_and_dangling_ids_are_rejected() {
        let mut s = spec_with(vec![node("a", &[]), node("a", &[])]);
        assert!(s.validate().unwrap_err().to_string().contains("duplicate"));

        s = spec_with(vec![node("a", &["ghost"])]);
        assert!(s.validate().unwrap_err().to_string().contains("unknown"));

        s = spec_with(vec![node("a", &[])]);
        s.nodes[0].parts.push(PartSpec {
            id: "p".into(),
            title: "again".into(),
            xp: 1,
            evidence: Evidence {
                manual: true,
                ..Default::default()
            },
        });
        assert!(s
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate part id"));
    }

    #[test]
    fn thresholds_default_to_one_commit_only_when_something_selects() {
        let sel = Evidence {
            paths: vec!["src/**".into()],
            ..Default::default()
        };
        assert_eq!(sel.required_commits(), 1);

        // A PR-only rule must not also demand a commit nobody asked for.
        let pr_only = Evidence {
            prs: Some(2),
            ..Default::default()
        };
        assert_eq!(pr_only.required_commits(), 0);
        assert_eq!(pr_only.required_prs(), 2);

        let manual = Evidence {
            manual: true,
            ..Default::default()
        };
        assert_eq!(manual.required_commits(), 0);
    }
}
