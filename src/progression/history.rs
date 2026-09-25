// SPDX-License-Identifier: Apache-2.0
//! The repository half — commits, the files they touched, and the PR each one
//! merged.
//!
//! Pure `gix`, like [`crate::diff`]: no `git` binary, so this runs on the same
//! static-binary CI step the gate already ships and works air-gapped.
//!
//! Two decisions worth stating, because they decide what the tree can claim:
//!
//! * **Merge commits are skipped.** Their first-parent diff re-attributes every
//!   file on the merged branch to the merge, which would close a milestone on
//!   the strength of one merge commit that "touched" a thousand paths. The
//!   branch's own commits are in the walk anyway.
//! * **PR numbers are parsed from the subject**, not fetched. A squash-merge
//!   repository writes `… (#1234)` and that is enough to count distinct PRs
//!   offline. No token, no rate limit, no network on a docs job. Repos that
//!   merge without a marker simply have no PR evidence, and a spec for such a
//!   repo should count commits instead.

use std::collections::HashSet;
use std::ops::ControlFlow;
use std::path::Path;

use anyhow::{Context, Result};
use gix::bstr::ByteSlice;

/// One commit, reduced to what the resolver can match on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    /// First line of the message.
    pub subject: String,
    /// Full message, so a `subject:` rule can also match a trailer.
    pub message: String,
    pub author: String,
    pub timestamp_unix: u64,
    /// Repo-relative paths this commit changed against its first parent.
    pub paths: Vec<String>,
    /// PR number parsed out of the subject, when the repo marks them.
    pub pr: Option<u64>,
}

impl Commit {
    /// Short sha, the form every UI shows.
    pub fn short(&self) -> &str {
        &self.sha[..self.sha.len().min(7)]
    }
}

/// What to walk.
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// Revspec to start from.
    pub head: String,
    /// Drop commits older than this (unix seconds).
    pub since_unix: Option<u64>,
    /// Stop after this many commits. A guard, not a feature: a progression
    /// refresh runs on every push, and an unbounded walk of a decade-old
    /// monorepo is a CI step that times out rather than a tree that is wrong.
    pub max_commits: usize,
}

impl Default for WalkOptions {
    fn default() -> Self {
        WalkOptions {
            head: "HEAD".to_string(),
            since_unix: None,
            max_commits: 20_000,
        }
    }
}

/// What the walk saw, beside the commits themselves.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkStats {
    /// Commits visited and kept.
    pub commits: usize,
    /// The walk stopped at `max_commits` with ancestry still unread.
    ///
    /// Set only when the iterator had another item to give, so a history of exactly
    /// `max_commits` commits — which ends on its own — is not flagged. Same failure shape as
    /// `shallow_boundary`: everything past the cap is invisible, and a tree resolved without it
    /// understates milestones while looking perfectly healthy.
    pub truncated: bool,
    /// Merge commits skipped (see the module docs).
    pub merges_skipped: usize,
    /// Commits whose parent object is missing — the boundary of a **shallow**
    /// clone. Load-bearing: a shallow checkout is `actions/checkout`'s default,
    /// and resolving a plan against one understates every milestone while
    /// looking perfectly healthy. The CLI refuses to write an artifact from
    /// such a walk unless told to.
    pub shallow_boundary: usize,
}

/// A walk's commits plus what it could not see.
#[derive(Debug, Clone, Default)]
pub struct History {
    pub commits: Vec<Commit>,
    pub stats: WalkStats,
}

/// Walk `head`'s ancestry, newest first, collecting matchable commits.
pub fn walk(repo_path: &Path, opts: &WalkOptions, pr_pattern: &regex::Regex) -> Result<History> {
    let repo = gix::open(repo_path)
        .with_context(|| format!("opening git repository at {}", repo_path.display()))?;

    let tip = repo
        .rev_parse_single(opts.head.as_str())
        .with_context(|| format!("resolving `{}` (is the history fetched?)", opts.head))?
        .object()
        .with_context(|| format!("reading object for `{}`", opts.head))?
        .peel_to_kind(gix::object::Kind::Commit)
        .with_context(|| format!("`{}` does not point at a commit", opts.head))?
        .into_commit()
        .id;

    let walk = repo
        .rev_walk([tip])
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
        ))
        .all()
        .context("starting the history walk")?;

    let mut out = Vec::new();
    let mut stats = WalkStats::default();
    for info in walk {
        if out.len() >= opts.max_commits {
            // Reached here only because the iterator yielded another commit past the cap.
            stats.truncated = true;
            break;
        }
        let info = info.context("walking history")?;
        let commit = repo
            .find_commit(info.id)
            .with_context(|| format!("reading commit {}", info.id))?;

        let time = commit.time().context("reading commit time")?;
        let ts = time.seconds.max(0) as u64;
        // `ByCommitTime` is newest-first, so once we are past the window every
        // remaining commit is older too — except that committer clocks are not
        // monotonic across rebases, so `continue` rather than `break`.
        if opts.since_unix.is_some_and(|since| ts < since) {
            continue;
        }

        let parents: Vec<gix::ObjectId> = commit.parent_ids().map(|id| id.detach()).collect();
        if parents.len() > 1 {
            stats.merges_skipped += 1;
            continue; // merge commit — see the module docs
        }

        let message = commit.message_raw_sloppy().to_str_lossy().into_owned();
        let subject = message
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        let author = commit
            .author()
            .map(|a| a.name.to_str_lossy().into_owned())
            .unwrap_or_default();

        // A missing parent object is the shallow boundary, not a broken repo.
        // Counting the commit with an empty path set would be worse than
        // dropping it: an empty set matches no rule, but it would still be
        // reported as a commit the walk understood.
        let paths = match changed_paths(&repo, &commit, parents.first().copied()) {
            Ok(paths) => paths,
            Err(ChangedPathsError::ParentMissing) => {
                stats.shallow_boundary += 1;
                continue;
            }
            Err(ChangedPathsError::Other(e)) => {
                return Err(e).with_context(|| format!("diffing commit {}", info.id))
            }
        };

        let pr = pr_pattern
            .captures(&subject)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<u64>().ok());

        out.push(Commit {
            sha: info.id.to_string(),
            subject,
            message,
            author,
            timestamp_unix: ts,
            paths,
            pr,
        });
        stats.commits += 1;
    }
    Ok(History {
        commits: out,
        stats,
    })
}

/// Why a commit's change set could not be read.
enum ChangedPathsError {
    /// The first parent is not in the object database — a shallow boundary.
    ParentMissing,
    Other(anyhow::Error),
}

/// Paths a commit changed against its first parent (everything, for a root commit).
fn changed_paths(
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
    parent: Option<gix::ObjectId>,
) -> std::result::Result<Vec<String>, ChangedPathsError> {
    let head_tree = commit.tree().map_err(|e| {
        ChangedPathsError::Other(anyhow::Error::new(e).context("reading commit tree"))
    })?;
    let base_tree = match parent {
        Some(id) => match repo.find_commit(id) {
            Ok(parent) => parent.tree().map_err(|e| {
                ChangedPathsError::Other(anyhow::Error::new(e).context("reading parent tree"))
            })?,
            Err(_) => return Err(ChangedPathsError::ParentMissing),
        },
        // A root commit has no parent; diff against the empty tree so its files
        // count as added rather than the commit being invisible to every rule.
        None => repo.empty_tree(),
    };

    let mut paths = Vec::new();
    let mut platform = base_tree.changes().map_err(|e| {
        ChangedPathsError::Other(anyhow::Error::new(e).context("starting tree diff"))
    })?;
    platform.options(|opts| {
        opts.track_path();
    });
    platform
        .for_each_to_obtain_tree(&head_tree, |change| {
            paths.push(change.location().to_str_lossy().into_owned());
            Ok::<_, std::convert::Infallible>(ControlFlow::Continue(()))
        })
        .map_err(|e| {
            ChangedPathsError::Other(
                anyhow::Error::new(e).context("diffing against the parent tree"),
            )
        })?;

    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Distinct PR numbers in a commit slice, ascending. Used for the fleet
/// readout ("this milestone took 6 PRs").
pub fn distinct_prs(commits: &[&Commit]) -> Vec<u64> {
    let mut seen: Vec<u64> = commits
        .iter()
        .filter_map(|c| c.pr)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    seen.sort_unstable();
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str, pr: Option<u64>) -> Commit {
        Commit {
            sha: sha.into(),
            subject: "s".into(),
            message: "s".into(),
            author: "a".into(),
            timestamp_unix: 0,
            paths: Vec::new(),
            pr,
        }
    }

    #[test]
    fn distinct_prs_dedups_and_sorts() {
        let cs = [
            commit("a", Some(7)),
            commit("b", Some(3)),
            commit("c", Some(7)),
            commit("d", None),
        ];
        let refs: Vec<&Commit> = cs.iter().collect();
        assert_eq!(distinct_prs(&refs), vec![3, 7]);
    }

    #[test]
    fn the_default_pr_pattern_reads_a_squash_subject() {
        let re = regex::Regex::new(super::super::spec::DEFAULT_PR_PATTERN).unwrap();
        let grab = |s: &str| {
            re.captures(s)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse::<u64>().ok())
        };
        assert_eq!(grab("dagpane: serve the frontier (#1165)"), Some(1165));
        // An issue reference mid-subject is not a merge marker, but the pattern
        // is deliberately loose — a repo that wants strictness overrides it.
        assert_eq!(grab("fix: no marker here"), None);
    }

    #[test]
    fn short_sha_is_seven_and_never_panics_on_a_stub() {
        assert_eq!(commit("0123456789ab", None).short(), "0123456");
        assert_eq!(commit("abc", None).short(), "abc");
    }
}
