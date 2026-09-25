//! turnover-history — the git side. Lists the commits to classify, loads both sides of every
//! changed file for one commit, and drives `turnover-core` over all of them in parallel.
//!
//! Pure Rust via `gix`: no `git` binary on the CI runner, no libgit2 to link, one static
//! binary that also runs air-gapped. Commits are independent once listed — every one is
//! "diff my tree against my first parent's" — so the walk is rayon over the commit list with
//! a thread-local repository handle per worker. The listing itself is sequential and cheap.
//!
//! What this crate decides, and the classifier never sees:
//!
//! * merge commits are **skipped by default** — their first-parent diff re-attributes the
//!   whole merged branch to one commit and one author, double-counting every branch commit
//!   the walk already visited;
//! * binary blobs, oversized blobs and paths under vendored/generated directories are
//!   dropped before classification, and counted in [`Stats`] so the report can say so;
//! * a detected **copy** (gix's copy detection over the commit's modified set) becomes an
//!   *addition* plus the source file as untouched context, so the classifier can see the
//!   paste. A rename is a modification of the renamed content.

use std::sync::atomic::{AtomicUsize, Ordering};

use gix::bstr::ByteSlice;
use gix::hash::ObjectId;
use gix::ThreadSafeRepository;
use rayon::prelude::*;
use turnover_core::attribution::{self, AttributionConfig, Origin};
use turnover_core::churn::{self, PendingAddition};
use turnover_core::line::line_count;
use turnover_core::signals::{
    classify, explain, CommitInput, CommitSignals, FileChange, FileExplanation, SignalConfig,
};
use turnover_core::Language;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("open repository {path}: {reason}")]
    Open { path: String, reason: String },
    #[error("resolve {spec:?}: {reason}")]
    Resolve { spec: String, reason: String },
    #[error("walk history: {0}")]
    Walk(String),
    #[error("read object {id}: {reason}")]
    Object { id: String, reason: String },
}

/// Which commits to walk and which files to count.
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// Revspecs to start from. Default `["HEAD"]`.
    pub tips: Vec<String>,
    /// Revspecs whose ancestry is excluded (a base branch, or the previous baseline head).
    pub hidden: Vec<String>,
    /// Hidden revspecs that fail to resolve are skipped instead of failing the walk — a
    /// baseline head that was rewritten away must not brick every later run.
    pub lenient_hidden: bool,
    /// Follow only first parents.
    pub first_parent: bool,
    /// Do not classify merge commits (see the crate docs).
    pub skip_merges: bool,
    /// Ignore commits older than this (unix seconds).
    pub since_unix: Option<i64>,
    /// Blobs larger than this are not classified.
    pub max_file_bytes: usize,
    /// Count text files whose extension has no [`Language`] as `Language::Other`.
    pub include_other: bool,
    /// Path segments that exclude a file (`vendor`, `node_modules`, …) and path prefixes.
    pub ignore_paths: Vec<String>,
    /// What marks a commit as AI-coauthored.
    pub attribution: AttributionConfig,
}

impl Default for WalkOptions {
    fn default() -> Self {
        WalkOptions {
            tips: vec!["HEAD".to_string()],
            hidden: Vec::new(),
            lenient_hidden: false,
            first_parent: false,
            skip_merges: true,
            since_unix: None,
            max_file_bytes: 1 << 20,
            include_other: false,
            ignore_paths: default_ignores(),
            attribution: AttributionConfig::default(),
        }
    }
}

/// The directories nobody wants counted as their own code.
pub fn default_ignores() -> Vec<String> {
    [
        "vendor",
        "node_modules",
        "third_party",
        "thirdparty",
        "dist",
        "build",
        "target",
        "generated",
        "__snapshots__",
        ".git",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// A commit the walk selected. `parent` is the first parent (the diff base).
#[derive(Debug, Clone)]
pub struct CommitMeta {
    pub id: ObjectId,
    pub parent: Option<ObjectId>,
    pub is_merge: bool,
    pub timestamp_unix: i64,
    /// The author's email — the identity rows are grouped by.
    pub author: String,
    /// AI-coauthored or human, decided from the author and the message at listing time.
    pub origin: Origin,
}

fn meta_of(
    repo: &gix::Repository,
    id: ObjectId,
    attribution: &AttributionConfig,
) -> Result<CommitMeta, Error> {
    let commit = repo.find_commit(id).map_err(|e| Error::Object {
        id: id.to_string(),
        reason: e.to_string(),
    })?;
    let timestamp_unix = commit
        .time()
        .map_err(|e| Error::Object {
            id: id.to_string(),
            reason: e.to_string(),
        })?
        .seconds;
    let (author_name, author_email) = commit
        .author()
        .map(|a| {
            (
                a.name.to_str_lossy().into_owned(),
                a.email.to_str_lossy().into_owned(),
            )
        })
        .unwrap_or_default();
    let message = commit.message_raw_sloppy().to_str_lossy().into_owned();
    let origin = attribution::classify(attribution, &author_name, &author_email, &message);
    let parents: Vec<ObjectId> = commit.parent_ids().map(|id| id.detach()).collect();
    Ok(CommitMeta {
        id,
        parent: parents.first().copied(),
        is_merge: parents.len() > 1,
        timestamp_unix,
        author: author_email,
        origin,
    })
}

/// What the walk dropped or degraded, for the report.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Stats {
    pub commits: usize,
    pub merges_skipped: usize,
    pub files: usize,
    pub files_ignored_path: usize,
    pub files_unsupported: usize,
    pub files_binary: usize,
    pub files_too_large: usize,
    /// Lines whose mask came from a grammar vs the heuristic — how much of the number is
    /// resting on unparsed text.
    pub lines_parsed: usize,
    pub lines_heuristic: usize,
}

#[derive(Default)]
struct AtomicStats {
    merges_skipped: AtomicUsize,
    files: AtomicUsize,
    files_ignored_path: AtomicUsize,
    files_unsupported: AtomicUsize,
    files_binary: AtomicUsize,
    files_too_large: AtomicUsize,
    lines_parsed: AtomicUsize,
    lines_heuristic: AtomicUsize,
}

impl AtomicStats {
    fn bump(&self, field: &AtomicUsize, n: usize) {
        field.fetch_add(n, Ordering::Relaxed);
    }
    fn snapshot(&self, commits: usize) -> Stats {
        Stats {
            commits,
            merges_skipped: self.merges_skipped.load(Ordering::Relaxed),
            files: self.files.load(Ordering::Relaxed),
            files_ignored_path: self.files_ignored_path.load(Ordering::Relaxed),
            files_unsupported: self.files_unsupported.load(Ordering::Relaxed),
            files_binary: self.files_binary.load(Ordering::Relaxed),
            files_too_large: self.files_too_large.load(Ordering::Relaxed),
            lines_parsed: self.lines_parsed.load(Ordering::Relaxed),
            lines_heuristic: self.lines_heuristic.load(Ordering::Relaxed),
        }
    }
}

/// Open a repository (worktree or bare) for use from many threads.
pub fn open(path: &str) -> Result<ThreadSafeRepository, Error> {
    ThreadSafeRepository::discover(path).map_err(|e| Error::Open {
        path: path.to_string(),
        reason: e.to_string(),
    })
}

fn resolve(repo: &gix::Repository, spec: &str) -> Result<ObjectId, Error> {
    repo.rev_parse_single(spec)
        .map(|id| id.detach())
        .map_err(|e| Error::Resolve {
            spec: spec.to_string(),
            reason: e.to_string(),
        })
}

/// Resolve a revspec (`HEAD`, a branch, a sha prefix) to its full object id.
pub fn resolve_sha(shared: &ThreadSafeRepository, spec: &str) -> Result<String, Error> {
    let repo = shared.to_thread_local();
    Ok(resolve(&repo, spec)?.to_string())
}

/// The commits reachable from `opts.tips` and not from `opts.hidden`, newest first.
pub fn list_commits(
    shared: &ThreadSafeRepository,
    opts: &WalkOptions,
) -> Result<Vec<CommitMeta>, Error> {
    use gix::revision::walk::Sorting;
    use gix::traverse::commit::simple::CommitTimeOrder;

    let repo = shared.to_thread_local();
    let tips = opts
        .tips
        .iter()
        .map(|s| resolve(&repo, s))
        .collect::<Result<Vec<_>, _>>()?;
    let mut hidden = Vec::new();
    for spec in &opts.hidden {
        match resolve(&repo, spec) {
            Ok(id) => hidden.push(id),
            Err(e) if opts.lenient_hidden => {
                eprintln!("turnover: ignoring unresolvable hidden rev {spec:?}: {e}");
            }
            Err(e) => return Err(e),
        }
    }
    let sorting = match opts.since_unix {
        Some(seconds) => Sorting::ByCommitTimeCutoff {
            order: CommitTimeOrder::NewestFirst,
            seconds,
        },
        None => Sorting::ByCommitTime(CommitTimeOrder::NewestFirst),
    };
    let mut platform = repo.rev_walk(tips).with_hidden(hidden).sorting(sorting);
    if opts.first_parent {
        platform = platform.first_parent_only();
    }
    let walk = platform.all().map_err(|e| Error::Walk(e.to_string()))?;
    let mut out = Vec::new();
    for info in walk {
        let info = info.map_err(|e| Error::Walk(e.to_string()))?;
        let meta = meta_of(&repo, info.id, &opts.attribution)?;
        if let Some(since) = opts.since_unix {
            if meta.timestamp_unix < since {
                continue;
            }
        }
        out.push(meta);
    }
    Ok(out)
}

fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|&b| b == 0)
}

fn path_ignored(path: &str, ignores: &[String]) -> bool {
    ignores.iter().any(|ig| {
        let ig = ig.trim_end_matches('/');
        path.starts_with(&format!("{ig}/")) || path.split('/').any(|seg| seg == ig)
    })
}

/// A text blob's content, or the reason it was dropped.
enum Blob {
    Text(String),
    Binary,
    TooLarge,
}

fn read_blob(repo: &gix::Repository, id: ObjectId, max: usize) -> Result<Blob, Error> {
    let blob = repo.find_blob(id).map_err(|e| Error::Object {
        id: id.to_string(),
        reason: e.to_string(),
    })?;
    let data: &[u8] = &blob.data;
    if data.len() > max {
        return Ok(Blob::TooLarge);
    }
    if is_binary(data) {
        return Ok(Blob::Binary);
    }
    Ok(Blob::Text(String::from_utf8_lossy(data).into_owned()))
}

fn language_for(path: &str, include_other: bool) -> Option<Language> {
    match Language::from_path(path) {
        Some(l) => Some(l),
        None if include_other => Some(Language::Other),
        None => None,
    }
}

/// Load every countable file of one commit with both sides and their masks.
fn load_commit(
    repo: &gix::Repository,
    meta: &CommitMeta,
    opts: &WalkOptions,
    stats: &AtomicStats,
) -> Result<CommitInput, Error> {
    use gix::object::tree::diff::ChangeDetached as Change;

    let err = |e: &dyn std::fmt::Display| Error::Object {
        id: meta.id.to_string(),
        reason: e.to_string(),
    };
    let commit = repo.find_commit(meta.id).map_err(|e| err(&e))?;
    let tree = commit.tree().map_err(|e| err(&e))?;
    let parent_tree = match meta.parent {
        Some(p) => Some(
            repo.find_commit(p)
                .map_err(|e| err(&e))?
                .tree()
                .map_err(|e| err(&e))?,
        ),
        None => None,
    };
    let rewrites = gix::diff::Rewrites {
        copies: Some(gix::diff::rewrites::Copies {
            source: gix::diff::rewrites::CopySource::FromSetOfModifiedFiles,
            percentage: Some(0.5),
        }),
        percentage: Some(0.5),
        limit: 1000,
        track_empty: false,
    };
    let diff_opts = gix::diff::Options::default().with_rewrites(Some(rewrites));
    let changes = repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), diff_opts)
        .map_err(|e| err(&e))?;

    let mut files = Vec::new();
    // (path, old id, new id, context-only)
    let mut entries: Vec<(String, Option<ObjectId>, Option<ObjectId>, bool)> = Vec::new();
    for change in changes {
        match change {
            Change::Addition {
                location,
                entry_mode,
                id,
                ..
            } => {
                if entry_mode.is_blob() {
                    entries.push((location.to_str_lossy().into_owned(), None, Some(id), false));
                }
            }
            Change::Deletion {
                location,
                entry_mode,
                id,
                ..
            } => {
                if entry_mode.is_blob() {
                    entries.push((location.to_str_lossy().into_owned(), Some(id), None, false));
                }
            }
            Change::Modification {
                location,
                previous_id,
                id,
                entry_mode,
                previous_entry_mode,
            } => {
                let old = previous_entry_mode.is_blob().then_some(previous_id);
                let new = entry_mode.is_blob().then_some(id);
                if old.is_some() || new.is_some() {
                    entries.push((location.to_str_lossy().into_owned(), old, new, false));
                }
            }
            Change::Rewrite {
                source_location,
                source_id,
                id,
                location,
                copy,
                entry_mode,
                source_entry_mode,
                ..
            } => {
                if !entry_mode.is_blob() {
                    continue;
                }
                let path = location.to_str_lossy().into_owned();
                if copy {
                    entries.push((path, None, Some(id), false));
                    if source_entry_mode.is_blob() {
                        entries.push((
                            source_location.to_str_lossy().into_owned(),
                            Some(source_id),
                            Some(source_id),
                            true,
                        ));
                    }
                } else {
                    entries.push((
                        path,
                        source_entry_mode.is_blob().then_some(source_id),
                        Some(id),
                        false,
                    ));
                }
            }
        }
    }

    for (path, old_id, new_id, context_only) in entries {
        stats.bump(&stats.files, 1);
        if path_ignored(&path, &opts.ignore_paths) {
            stats.bump(&stats.files_ignored_path, 1);
            continue;
        }
        let Some(language) = language_for(&path, opts.include_other) else {
            stats.bump(&stats.files_unsupported, 1);
            continue;
        };
        let side = |id: Option<ObjectId>| -> Result<Option<Option<String>>, Error> {
            match id {
                None => Ok(Some(None)),
                Some(id) => match read_blob(repo, id, opts.max_file_bytes)? {
                    Blob::Text(t) => Ok(Some(Some(t))),
                    Blob::Binary => {
                        stats.bump(&stats.files_binary, 1);
                        Ok(None)
                    }
                    Blob::TooLarge => {
                        stats.bump(&stats.files_too_large, 1);
                        Ok(None)
                    }
                },
            }
        };
        let (Some(old), Some(new)) = (side(old_id)?, side(new_id)?) else {
            continue;
        };
        let (old, new) = if context_only {
            (new.clone(), new)
        } else {
            (old, new)
        };
        let mask_of = |text: &Option<String>| -> Option<Vec<bool>> {
            text.as_ref().map(|t| {
                let (m, src) = turnover_lang::mask_for_path(language, &path, t);
                match src {
                    turnover_lang::MaskSource::Parsed => {
                        stats.bump(&stats.lines_parsed, line_count(t))
                    }
                    turnover_lang::MaskSource::Heuristic => {
                        stats.bump(&stats.lines_heuristic, line_count(t))
                    }
                }
                m
            })
        };
        let old_mask = mask_of(&old);
        let new_mask = mask_of(&new);
        files.push(FileChange {
            path,
            language,
            old,
            new,
            old_mask,
            new_mask,
        });
    }

    Ok(CommitInput {
        sha: meta.id.to_string(),
        parent: meta.parent.map(|p| p.to_string()),
        timestamp_unix: meta.timestamp_unix,
        author: meta.author.clone(),
        is_merge: meta.is_merge,
        origin: meta.origin,
        files,
    })
}

/// Classify every listed commit in parallel. `progress` is called with the running count.
pub fn classify_all(
    shared: &ThreadSafeRepository,
    metas: &[CommitMeta],
    opts: &WalkOptions,
    cfg: &SignalConfig,
    progress: &(dyn Fn(usize) + Sync),
) -> Result<(Vec<CommitSignals>, Stats), Error> {
    let stats = AtomicStats::default();
    let done = AtomicUsize::new(0);
    let rows: Vec<CommitSignals> = metas
        .par_iter()
        .map_init(
            || {
                let mut repo = shared.to_thread_local();
                repo.object_cache_size_if_unset(64 * 1024 * 1024);
                repo
            },
            |repo, meta| {
                let row = if meta.is_merge && opts.skip_merges {
                    stats.bump(&stats.merges_skipped, 1);
                    Ok(CommitSignals {
                        sha: meta.id.to_string(),
                        parent: meta.parent.map(|p| p.to_string()),
                        timestamp_unix: meta.timestamp_unix,
                        author: meta.author.clone(),
                        is_merge: true,
                        origin: meta.origin,
                        ..Default::default()
                    })
                } else {
                    load_commit(repo, meta, opts, &stats).map(|input| classify(&input, cfg))
                };
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                progress(n);
                row
            },
        )
        .collect::<Result<Vec<_>, _>>()?;
    Ok((rows, stats.snapshot(metas.len())))
}

/// Attribute churn over `rows` (new) merged with `existing` (already in a baseline), seeded
/// by `pending`; returns the still-open additions for the next refresh.
pub fn attribute_churn(
    existing: &mut Vec<CommitSignals>,
    rows: Vec<CommitSignals>,
    pending: &[PendingAddition],
    horizon_secs: i64,
) -> Vec<PendingAddition> {
    existing.extend(rows);
    churn::attribute(existing, pending, horizon_secs)
}

/// Load and explain one commit — `turnover explain`.
pub fn explain_commit(
    shared: &ThreadSafeRepository,
    spec: &str,
    opts: &WalkOptions,
    cfg: &SignalConfig,
) -> Result<(CommitSignals, Vec<FileExplanation>, Stats), Error> {
    let repo = shared.to_thread_local();
    let id = resolve(&repo, spec)?;
    let meta = meta_of(&repo, id, &opts.attribution)?;
    let stats = AtomicStats::default();
    let input = load_commit(&repo, &meta, opts, &stats)?;
    let (row, files) = explain(&input, cfg);
    Ok((row, files, stats.snapshot(1)))
}

/// Explain several listed commits in parallel — the per-file evidence behind a gate verdict.
pub fn explain_commits(
    shared: &ThreadSafeRepository,
    metas: &[CommitMeta],
    opts: &WalkOptions,
    cfg: &SignalConfig,
) -> Result<Vec<(CommitSignals, Vec<FileExplanation>)>, Error> {
    let stats = AtomicStats::default();
    metas
        .par_iter()
        .map_init(
            || {
                let mut repo = shared.to_thread_local();
                repo.object_cache_size_if_unset(64 * 1024 * 1024);
                repo
            },
            |repo, meta| {
                if meta.is_merge && opts.skip_merges {
                    return Ok(None);
                }
                let input = load_commit(repo, meta, opts, &stats)?;
                Ok(Some(explain(&input, cfg)))
            },
        )
        .collect::<Result<Vec<_>, _>>()
        .map(|v| v.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_rules_match_segments_and_prefixes() {
        let ig = default_ignores();
        assert!(path_ignored("vendor/x/y.go", &ig));
        assert!(path_ignored("web/node_modules/a/b.js", &ig));
        assert!(!path_ignored("src/vendor_api.rs", &ig));
        assert!(path_ignored("docs/gen/a.rs", &["docs/gen".to_string()]));
    }

    #[test]
    fn binary_detection_is_a_nul_byte_in_the_head() {
        assert!(is_binary(b"abc\0def"));
        assert!(!is_binary("plain text\n".as_bytes()));
    }
}
