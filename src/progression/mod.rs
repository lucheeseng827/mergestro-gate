// SPDX-License-Identifier: Apache-2.0
//! **Progression** — the repository's plan, drawn as a tree and closed by its
//! own history.
//!
//! The gate answers "is this change safe to merge?". Progression answers the
//! question next to it, which no single CI run can: *where are we against the
//! plan we wrote?* A quarter plan, a phased build or a sprint is authored once
//! as a [`spec::ProgressionSpec`] — milestones, what each depends on, and the
//! parts each breaks into — and every part carries an **evidence rule** that
//! the repository itself satisfies: commits under these paths, this many
//! distinct PRs, a subject matching this pattern.
//!
//! Resolving the spec against history produces a
//! [`resolve::ProgressionSnapshot`]: every milestone with a state
//! (locked → ready → in progress → done), XP earned against XP available, the
//! commits and PRs that got it there, and a layout so every renderer draws the
//! same tree in the same arrangement.
//!
//! Three consumers, one snapshot:
//!
//! * `slop-gate progression --svg` writes the committed drawing, and
//!   `--readme` keeps the block between the
//!   [`render::MARKER_START`]/[`render::MARKER_END`] markers in step with it.
//!   `--check` makes both a CI assertion instead of a commit.
//! * `--json` writes the machine contract, which is also the body a repo
//!   uploads to the Mergestro control plane so the console can draw the tree on
//!   its canvas across every repo in a fleet.
//! * `--format text` prints the summary a developer reads in a job log.
//!
//! ## Why "gamified" is not decoration here
//!
//! A plan document tells you what was intended and a commit log tells you what
//! happened; neither tells you how far apart they are. Stating the distance as
//! a level and a bar is not a scoreboard for its own sake — it is the one
//! framing in which "we are 40% through a plan whose last third is untouched"
//! is legible at a glance, and in which a milestone nobody has started stays
//! visible instead of being quietly dropped from the next planning doc.
//!
//! Which is also why evidence rules are strict: a part with no selector closes
//! nothing ([`spec::Evidence`]), and a milestone cannot be `done` while
//! something it requires is open. A progress number that can be gamed by
//! committing is worse than no number.

pub mod history;
pub mod init;
pub mod record;
pub mod render;
pub mod resolve;
pub mod spec;

use std::path::Path;

use anyhow::{Context, Result};

pub use history::{Commit, History, WalkOptions, WalkStats};
pub use init::{draft, Draft, DraftSource, InitOptions};
pub use record::{Identity as RecordIdentity, ProgressionRecord};
pub use render::{
    inject_readme, render_markdown, render_svg, render_text, MARKER_END, MARKER_START,
};
pub use resolve::{
    EvidenceRef, NodeProgress, NodeState, PartProgress, ProgressionSnapshot, ProgressionTotals,
};
pub use spec::{Evidence, NodeSpec, PartSpec, ProgressionSpec};

/// Resolve `spec` against the repository at `repo_path`, at `head`.
///
/// The one entry point the CLI and any embedder needs: it opens the repo,
/// walks the window the spec asks for, and resolves.
///
/// The [`WalkStats`] come back with the snapshot rather than being logged away,
/// because one of them — `shallow_boundary` — decides whether the snapshot is
/// trustworthy at all — as does `truncated`. A caller that writes a committed artifact must look
/// at both; see `slop-gate progression --allow-shallow` and `--max-commits`.
pub fn snapshot(
    repo_path: &Path,
    spec: &ProgressionSpec,
    head: &str,
    now_unix: u64,
    max_commits: Option<usize>,
) -> Result<(ProgressionSnapshot, WalkStats)> {
    let pr_pattern = regex::Regex::new(&spec.pr_pattern)
        .with_context(|| format!("compiling `pr_pattern` {:?}", spec.pr_pattern))?;
    let opts = WalkOptions {
        head: head.to_string(),
        since_unix: spec
            .since_days
            .map(|d| now_unix.saturating_sub(d as u64 * 86_400)),
        max_commits: max_commits.unwrap_or_else(|| WalkOptions::default().max_commits),
    };
    let history = history::walk(repo_path, &opts, &pr_pattern)?;
    let head_sha = resolve_head_sha(repo_path, head)?;
    let snap = resolve::resolve(spec, &history.commits, &head_sha, now_unix)?;
    Ok((snap, history.stats))
}

/// The full sha `head` names, so the snapshot records what it was resolved at.
pub fn resolve_head_sha(repo_path: &Path, head: &str) -> Result<String> {
    let repo = gix::open(repo_path)
        .with_context(|| format!("opening git repository at {}", repo_path.display()))?;
    let id = repo
        .rev_parse_single(head)
        .with_context(|| format!("resolving `{head}`"))?
        .object()
        .with_context(|| format!("reading object for `{head}`"))?
        .peel_to_kind(gix::object::Kind::Commit)
        .with_context(|| format!("`{head}` does not point at a commit"))?
        .id;
    Ok(id.to_string())
}

/// Seconds since the epoch, or 0 on a clock before it.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
