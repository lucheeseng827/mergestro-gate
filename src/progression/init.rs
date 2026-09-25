// SPDX-License-Identifier: Apache-2.0
//! **`progression init`** — a first draft of the plan, mined from the repository.
//!
//! [`super::spec`] opens by saying that nothing in a spec is derived from the
//! repository: the repository only ever *closes* what the author declared. That
//! is still true of every spec this crate resolves, and it is the reason the
//! tree can describe intent at all. This module is the one place that runs the
//! other way, and deliberately only far enough to save typing.
//!
//! Two things history knows better than the author does:
//!
//! * **Which components exist.** A repo-relative glob that matches nothing is
//!   the most common way a first spec ends up flat at 0%, and nothing in the
//!   tool says so — an unmatched path and an untouched milestone render
//!   identically. Mining the directories out of the commits that touched them
//!   cannot produce a glob that matches nothing.
//! * **How merges are marked.** `(#123)` is GitHub's squash subject; GitLab
//!   writes `See merge request …!123`; a rebase-merge repo writes nothing at
//!   all and has no PR evidence to count. Getting [`ProgressionSpec::pr_pattern`]
//!   wrong is silent: every `prs:` threshold simply never closes.
//!
//! What it cannot do is write the plan. A milestone is a claim about intent and
//! intent is not in the log, so everything mined here describes work that has
//! **already landed** — the tree's roots, not its frontier. The draft says so
//! in its own comments and ends with one open milestone the author replaces.
//! That is also the honest answer to the retro-fit problem: a plan written
//! today for a five-year-old repository resolves to ~100% on the first run, and
//! pretending otherwise (a sliding `since_days`, thresholds invented above the
//! current count) buys a prettier number by making the tree mean less.
//!
//! The draft is never written until it has been parsed back and validated, so
//! this module cannot emit a file that `slop-gate progression` would reject.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};

use super::history::{Commit, WalkStats};
use super::spec::{Evidence, NodeSpec, PartSpec, ProgressionSpec, DEFAULT_PART_XP};

/// Milestones mined at most, before the frontier and phase markers.
pub const DEFAULT_MAX_NODES: usize = 8;
/// Commits a directory needs before it is worth a milestone of its own.
pub const DEFAULT_MIN_COMMITS: u32 = 3;
/// How many path segments deep a component may sit (`a/b/c`).
pub const DEFAULT_DEPTH: usize = 3;

/// A lone child must hold this share of its parent's commits to replace it.
const DOMINANT_PCT: usize = 80;
/// A sub-directory must hold this share of a component to become a part.
const PART_SHARE_PCT: usize = 10;
/// At most this many parts per mined milestone.
const MAX_PARTS: usize = 3;
/// A marker pattern must appear on this share of commits to be believed.
const PR_COVERAGE_PCT: usize = 10;
/// Below this many mined components the draft is a starter skeleton instead.
const MIN_COMPONENTS: usize = 2;

/// Directory segments that are build output or someone else's code.
const IGNORED_SEGMENTS: &[&str] = &[
    "target",
    "node_modules",
    "vendor",
    "dist",
    ".venv",
    "venv",
    "__pycache__",
    ".idea",
    ".vscode",
    ".mypy_cache",
    ".pytest_cache",
    "coverage",
];

/// Directory names that say what *kind* of file is inside rather than what work
/// it is. They are the natural parts of a component and never a component
/// themselves: a directory holding `src/` and `docs/` is one unit of work, and
/// a directory whose only child is `src/` is that child.
const CONTAINER_LEAVES: &[&str] = &[
    "src", "lib", "app", "source", "sources", "pkg", "cmd", "internal", "tests", "test", "spec",
    "specs", "docs", "doc", "include", "examples", "benches", "bench",
];

/// Files that ride along with every change and describe none of it.
const IGNORED_FILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "poetry.lock",
    "Gemfile.lock",
    "composer.lock",
    "go.sum",
    "uv.lock",
];

/// What to mine, and what to call the result.
#[derive(Debug, Clone)]
pub struct InitOptions {
    /// Plan title. The caller defaults this to the repository's directory name.
    pub title: String,
    /// Free-form period label written into the draft.
    pub season: Option<String>,
    /// Cap on mined milestones.
    pub max_nodes: usize,
    /// Commits a directory needs to qualify.
    pub min_commits: u32,
    /// Deepest component path (`a/b/c` is depth 3).
    pub depth: usize,
    /// Window the caller walked, written into the draft so a later resolve
    /// sees the same history the draft was mined from.
    pub since_days: Option<u32>,
}

impl Default for InitOptions {
    fn default() -> Self {
        InitOptions {
            title: "Progression".to_string(),
            season: None,
            max_nodes: DEFAULT_MAX_NODES,
            min_commits: DEFAULT_MIN_COMMITS,
            depth: DEFAULT_DEPTH,
            since_days: None,
        }
    }
}

/// Where the draft's milestones came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftSource {
    /// Mined from history: this many components over this many commits.
    Mined { components: usize, commits: usize },
    /// Too little history to mine — a conventional starter plan instead.
    Skeleton { reason: String },
}

/// The merge marker the repository actually writes.
#[derive(Debug, Clone)]
pub struct PrMarker {
    /// Regex written into the draft's `pr_pattern`.
    pub pattern: String,
    /// Human name for the comment: "GitHub squash merges".
    pub label: String,
    /// Share of walked commits carrying it, 0-100.
    pub coverage_pct: usize,
    /// Whether anything matched at all. When false the draft counts commits.
    pub found: bool,
}

/// One mined component: a directory that carries enough work to be a milestone.
#[derive(Debug, Clone)]
pub struct Component {
    /// Repo-relative directory, no trailing slash.
    pub path: String,
    /// Commits that touched it.
    pub commits: usize,
    /// Distinct files seen under it.
    pub files: usize,
    /// Oldest and newest commit that touched it.
    pub first_unix: u64,
    pub last_unix: u64,
    /// Sub-directories worth a part, biggest first.
    pub children: Vec<(String, usize)>,
}

/// A finished draft: the spec, the YAML that carries it, and what to tell the
/// author about how it was arrived at.
#[derive(Debug, Clone)]
pub struct Draft {
    pub spec: ProgressionSpec,
    pub yaml: String,
    pub source: DraftSource,
    pub marker: PrMarker,
    /// The components behind the mined milestones, in emitted order.
    pub components: Vec<Component>,
    /// Lines worth printing to whoever ran the command.
    pub notes: Vec<String>,
    /// The milestone the author is meant to replace, when the draft has one.
    /// The starter skeleton does not: every milestone in it is open already.
    pub frontier_id: Option<String>,
}

/// Mine `commits` into a draft plan.
///
/// `stats` decides how much the ordering can be trusted: a truncated or shallow
/// walk cannot say which component came first, so the draft falls back to a
/// flat tree rather than inventing phases out of an arbitrary window.
pub fn draft(
    commits: &[Commit],
    stats: &WalkStats,
    opts: &InitOptions,
    now_unix: u64,
) -> Result<Draft> {
    let marker = detect_pr_marker(commits);
    let components = discover(commits, opts);

    let ordering_trusted =
        !stats.truncated && stats.shallow_boundary == 0 && opts.since_days.is_none();

    let (spec, notes_meta, source, frontier_id): (_, _, _, Option<String>) =
        if components.len() < MIN_COMPONENTS {
            let reason = if commits.is_empty() {
                "no commits in the walked history".to_string()
            } else {
                format!(
                    "fewer than {} directories reached {} commits in the {} walked",
                    MIN_COMPONENTS,
                    opts.min_commits,
                    commits.len()
                )
            };
            let (spec, meta) = skeleton_spec(opts, &marker);
            (spec, meta, DraftSource::Skeleton { reason }, None)
        } else {
            let (spec, meta) = mined_spec(opts, &marker, &components, ordering_trusted);
            (
                spec,
                meta,
                DraftSource::Mined {
                    components: components.len(),
                    commits: commits.len(),
                },
                Some("next".to_string()),
            )
        };

    let yaml = render_yaml(&spec, &notes_meta, &source, &marker, opts, now_unix);

    // The draft is a file someone else's CI will read. Parse it back and run the
    // same validation `progression --spec` runs, so a bug in the emitter is this
    // command's failure rather than tomorrow's confusing CI error.
    let reparsed: ProgressionSpec = serde_yaml::from_str(&yaml)
        .context("internal error: the scaffolded YAML does not parse (please report)")?;
    reparsed
        .validate()
        .context("internal error: the scaffolded plan does not validate (please report)")?;

    let mut notes = Vec::new();
    match &source {
        DraftSource::Mined {
            components: n,
            commits: c,
        } => notes.push(format!(
            "mined {n} component(s) from {c} commit(s); {} milestone(s) in the draft",
            spec.nodes.len()
        )),
        DraftSource::Skeleton { reason } => notes.push(format!(
            "starter skeleton ({reason}) — the globs point at conventional locations, not mined ones"
        )),
    }
    if marker.found {
        notes.push(format!(
            "PR marker: {} on {}% of commits",
            marker.label, marker.coverage_pct
        ));
    } else if stats.merges_skipped > 0 {
        notes.push(format!(
            "no PR marker in any commit subject, and {} merge commit(s) were skipped — this repo \
             probably merges without squashing, so count commits, not PRs",
            stats.merges_skipped
        ));
    } else {
        notes.push(
            "no PR marker in any commit subject — the draft's evidence counts commits".to_string(),
        );
    }
    if !ordering_trusted && !matches!(source, DraftSource::Skeleton { .. }) {
        notes.push(
            "the walk was windowed or truncated, so \"which component came first\" is not knowable \
             — the draft is flat rather than phased"
                .to_string(),
        );
    }

    Ok(Draft {
        spec: reparsed,
        yaml,
        source,
        marker,
        components,
        notes,
        frontier_id,
    })
}

/// Candidate merge markers, most specific first.
///
/// Order is the whole algorithm: `#(\d+)` matches every subject the other three
/// do (and every issue reference besides), so picking by coverage would always
/// pick the loosest. The first pattern that clears [`PR_COVERAGE_PCT`] wins.
const PR_CANDIDATES: &[(&str, &str)] = &[
    (r"\(#(\d+)\)", "GitHub squash merges — `… (#123)`"),
    (
        r"[Ss]ee merge request [^\s]*!(\d+)",
        "GitLab merge requests — `See merge request …!123`",
    ),
    (r"(?i)\bpull request #(\d+)", "`pull request #123` subjects"),
    (r"(?i)\bPR[ -]?#?(\d+)", "`PR #123` subjects"),
    (r"#(\d+)", "a bare `#123` in the subject"),
];

/// Which marker this repository writes, if any.
pub fn detect_pr_marker(commits: &[Commit]) -> PrMarker {
    if commits.is_empty() {
        return PrMarker {
            pattern: super::spec::DEFAULT_PR_PATTERN.to_string(),
            label: "unknown (no commits walked)".to_string(),
            coverage_pct: 0,
            found: false,
        };
    }
    for (pattern, label) in PR_CANDIDATES {
        let Ok(re) = regex::Regex::new(pattern) else {
            continue;
        };
        let hits = commits.iter().filter(|c| re.is_match(&c.subject)).count();
        let pct = hits * 100 / commits.len();
        if pct >= PR_COVERAGE_PCT {
            return PrMarker {
                pattern: (*pattern).to_string(),
                label: (*label).to_string(),
                coverage_pct: pct,
                found: true,
            };
        }
    }
    PrMarker {
        pattern: super::spec::DEFAULT_PR_PATTERN.to_string(),
        label: "none found".to_string(),
        coverage_pct: 0,
        found: false,
    }
}

/// What one directory prefix accumulated across the walk.
#[derive(Debug, Default, Clone)]
struct Tally {
    commits: usize,
    files: HashSet<String>,
    first_unix: u64,
    last_unix: u64,
}

impl Tally {
    fn touch(&mut self, ts: u64) {
        if self.first_unix == 0 || ts < self.first_unix {
            self.first_unix = ts;
        }
        self.last_unix = self.last_unix.max(ts);
    }
}

/// `path`'s directory prefix at `depth` segments, or `None` when it is shallower.
///
/// `a/b/c.rs` is depth 2: `dir_prefix(_, 1) == "a"`, `dir_prefix(_, 2) == "a/b"`,
/// and depth 3 is `None` — the file itself is never a component.
fn dir_prefix(path: &str, depth: usize) -> Option<&str> {
    let mut seen = 0;
    for (i, ch) in path.char_indices() {
        if ch == '/' {
            seen += 1;
            if seen == depth {
                return Some(&path[..i]);
            }
        }
    }
    None
}

/// Whether a path is build output, a vendored tree, or a lockfile.
fn ignored(path: &str) -> bool {
    if path.is_empty() {
        return true;
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    if IGNORED_FILES.contains(&file) {
        return true;
    }
    path.split('/')
        .any(|seg| IGNORED_SEGMENTS.contains(&seg) || seg == ".git")
}

/// Commit counts per directory prefix, to `max_depth` segments.
///
/// A prefix is counted **once per commit** however many of its files changed:
/// otherwise a formatting sweep over 400 files outranks a year of work in one
/// directory, and the scaffolder would mine the repository's noisiest hour.
fn tally(commits: &[Commit], max_depth: usize) -> HashMap<String, Tally> {
    let mut out: HashMap<String, Tally> = HashMap::new();
    for commit in commits {
        let mut seen: HashSet<&str> = HashSet::new();
        for path in &commit.paths {
            if ignored(path) {
                continue;
            }
            for depth in 1..=max_depth {
                let Some(prefix) = dir_prefix(path, depth) else {
                    break;
                };
                let entry = out.entry(prefix.to_string()).or_default();
                if seen.insert(prefix) {
                    entry.commits += 1;
                }
                entry.files.insert(path.clone());
                entry.touch(commit.timestamp_unix);
            }
        }
    }
    out
}

/// Whether a prefix's last segment names a kind of file rather than a unit of work.
fn is_container(prefix: &str) -> bool {
    let leaf = prefix.rsplit('/').next().unwrap_or(prefix);
    CONTAINER_LEAVES.contains(&leaf)
}

/// Segments in a prefix: `a/b` is 2.
fn depth_of(prefix: &str) -> usize {
    prefix.split('/').count()
}

/// Prefixes exactly one segment below `parent`.
fn children_of<'a>(
    tallies: &'a HashMap<String, Tally>,
    parent: &str,
    min_commits: usize,
) -> Vec<(&'a String, &'a Tally)> {
    let want = depth_of(parent) + 1;
    let mut kids: Vec<(&String, &Tally)> = tallies
        .iter()
        .filter(|(p, t)| {
            t.commits >= min_commits
                && depth_of(p) == want
                && p.starts_with(parent)
                && p.as_bytes().get(parent.len()) == Some(&b'/')
        })
        .collect();
    // Commits desc, then path, so the cut does not depend on HashMap order.
    kids.sort_by(|a, b| b.1.commits.cmp(&a.1.commits).then(a.0.cmp(b.0)));
    kids
}

/// Choose the cut through the prefix tree that best describes the repository.
///
/// Two rules, and between them they handle both shapes that break a fixed
/// depth. **Split** a directory whose work is spread over two or more children
/// — that is a container of components (`services`, `packages`), and
/// naming it as one milestone says nothing. **Descend** through a lone child
/// that holds nearly all of its parent (`code` → `code/services`) —
/// that is a wrapper directory, and stopping there would hand every milestone
/// the same glob. Anything else is a component: kept whole, because a
/// sub-directory below the threshold is part of the work, not work of its own.
fn choose_cut(
    tallies: &HashMap<String, Tally>,
    max_depth: usize,
    min_commits: usize,
) -> Vec<String> {
    let mut queue: Vec<String> = tallies
        .iter()
        .filter(|(p, t)| depth_of(p) == 1 && t.commits >= min_commits)
        .map(|(p, _)| p.clone())
        .collect();
    queue.sort();

    let mut out = Vec::new();
    while let Some(prefix) = queue.pop() {
        let Some(tally) = tallies.get(&prefix) else {
            continue;
        };
        if depth_of(&prefix) >= max_depth {
            out.push(prefix);
            continue;
        }
        // `src` and `docs` are how one component is laid out, not two
        // components, so only children that NAME something can move the cut.
        let units: Vec<(&String, &Tally)> = children_of(tallies, &prefix, min_commits)
            .into_iter()
            .filter(|(p, _)| !is_container(p))
            .collect();
        match units.len() {
            1 if units[0].1.commits * 100 >= tally.commits * DOMINANT_PCT => {
                queue.push(units[0].0.clone());
            }
            0 | 1 => out.push(prefix),
            _ => queue.extend(units.into_iter().map(|(p, _)| p.clone())),
        }
    }
    out.sort();
    out
}

/// The components worth a milestone, in the order the repository grew them.
pub fn discover(commits: &[Commit], opts: &InitOptions) -> Vec<Component> {
    let depth = opts.depth.max(1);
    let min_commits = opts.min_commits.max(1) as usize;
    // One level past the cut, so a component's parts are already counted.
    let tallies = tally(commits, depth + 1);
    let cut = choose_cut(&tallies, depth, min_commits);

    let mut picked: Vec<Component> = cut
        .into_iter()
        .filter_map(|path| {
            let tally = tallies.get(&path)?;
            let children = children_of(&tallies, &path, min_commits)
                .into_iter()
                .filter(|(_, t)| t.commits * 100 >= tally.commits * PART_SHARE_PCT)
                .take(MAX_PARTS)
                .map(|(p, t)| (p.clone(), t.commits))
                .collect();
            Some(Component {
                path,
                commits: tally.commits,
                files: tally.files.len(),
                first_unix: tally.first_unix,
                last_unix: tally.last_unix,
                children,
            })
        })
        .collect();

    // Rank by weight to decide *which* survive the cap...
    picked.sort_by(|a, b| {
        b.commits
            .cmp(&a.commits)
            .then(b.files.cmp(&a.files))
            .then(a.path.cmp(&b.path))
    });
    picked.truncate(opts.max_nodes.max(1));
    // ...then by age to decide what order they are read in. The two are
    // different questions: the biggest component is rarely the first one.
    picked.sort_by(|a, b| a.first_unix.cmp(&b.first_unix).then(a.path.cmp(&b.path)));
    picked
}

/// Comment lines the emitter prints above a node, keyed by node id.
#[derive(Debug, Default, Clone)]
struct Meta {
    /// A section rule printed above the node, e.g. a phase heading.
    banner: HashMap<String, String>,
    /// Plain comment lines printed under the banner.
    lines: HashMap<String, Vec<String>>,
}

impl Meta {
    fn line(&mut self, id: &str, text: String) {
        self.lines.entry(id.to_string()).or_default().push(text);
    }
}

/// Lowercase identifier from an arbitrary path segment.
fn ident(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_underscore = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "node".to_string()
    } else if trimmed.starts_with(|c: char| c.is_ascii_digit()) {
        // Ids become SVG anchors and canvas keys; a leading digit is legal but
        // needlessly surprising in a selector.
        format!("n_{trimmed}")
    } else {
        trimmed
    }
}

/// An id from `path`'s last segment, widened with its parent on a collision.
fn unique_id(path: &str, used: &mut HashSet<String>) -> String {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut candidates = Vec::new();
    if let Some(last) = segs.last() {
        candidates.push(ident(last));
        if segs.len() >= 2 {
            candidates.push(ident(&segs[segs.len() - 2..].join("_")));
        }
    }
    candidates.push(ident(path));
    for candidate in candidates {
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    let base = ident(path);
    for n in 2.. {
        let candidate = format!("{base}_{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("the loop above returns")
}

/// Everything under `dir`, as a glob.
fn glob_under(dir: &str) -> String {
    format!("{}/**", dir.trim_end_matches('/'))
}

/// How many phases a tree of `n` mined components is worth.
///
/// Phases are ordered by when each component first appeared, so they are only
/// meaningful when the walk saw the whole history: inside a window every
/// component "first appeared" at the window's edge.
fn phase_count(n: usize, ordering_trusted: bool) -> usize {
    if !ordering_trusted {
        1
    } else if n >= 6 {
        3
    } else if n >= 4 {
        2
    } else {
        1
    }
}

/// Near-equal chunk sizes for `n` items in `k` groups, biggest first.
fn chunk_sizes(n: usize, k: usize) -> Vec<usize> {
    if k <= 1 {
        return vec![n];
    }
    let base = n / k;
    let extra = n % k;
    (0..k).map(|i| base + usize::from(i < extra)).collect()
}

/// `YYYY-MM-DD` from a unix timestamp (proleptic Gregorian, UTC).
fn ymd(ts: u64) -> String {
    let (y, m, d) = civil_from_unix(ts);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM` from a unix timestamp.
fn ym(ts: u64) -> String {
    let (y, m, _) = civil_from_unix(ts);
    format!("{y:04}-{m:02}")
}

/// Days-to-civil, Howard Hinnant's algorithm — the standard branch-free form.
///
/// A dependency-free date is worth fifteen lines here: the scaffold's comments
/// are read by a human months later, and "first touched 2024-03-02" survives
/// being re-read in a way "14 months ago" does not.
fn civil_from_unix(ts: u64) -> (i64, u32, u32) {
    let z = (ts / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The subject rule every frontier milestone carries.
///
/// It must be **open**, which rules out `manual: true` — that field closes a
/// part by hand, so a placeholder written with it would render as a finished
/// milestone the moment it was scaffolded. A subject rule nobody has written a
/// commit for is open, is a legal rule rather than a special case, and stays
/// visible on the canvas until somebody replaces it.
///
/// It is a bracketed tag rather than a word for a reason found by running this
/// on a real repository: the first version matched `REPLACE-ME`, one commit
/// body in 2,760 said "replace-me" in passing, and the frontier scaffolded
/// itself shut. A placeholder that prose can satisfy is not a placeholder. The
/// tag doubles as the escape hatch — tag a commit `[progression:next]` and the
/// milestone closes, which is the honest way to say "done, and I never got
/// round to writing the rule".
const FRONTIER_SUBJECT: &str = r"\[progression:next\]";

fn frontier_node(requires: Vec<String>) -> NodeSpec {
    NodeSpec {
        id: "next".to_string(),
        title: "What's next".to_string(),
        summary: Some(
            "The one milestone history cannot write. Replace it with the work you are planning."
                .to_string(),
        ),
        requires,
        paths: Vec::new(),
        parts: vec![PartSpec {
            id: "define".to_string(),
            title: "Name the work, then wire it to evidence".to_string(),
            xp: DEFAULT_PART_XP,
            evidence: Evidence {
                subject: Some(FRONTIER_SUBJECT.to_string()),
                ..Default::default()
            },
        }],
    }
}

/// Build the mined tree: components in the order they appeared, grouped into
/// phases, with one open milestone at the frontier.
fn mined_spec(
    opts: &InitOptions,
    marker: &PrMarker,
    components: &[Component],
    ordering_trusted: bool,
) -> (ProgressionSpec, Meta) {
    let mut meta = Meta::default();
    let mut used: HashSet<String> = HashSet::new();
    let mut nodes: Vec<NodeSpec> = Vec::new();

    let phases = phase_count(components.len(), ordering_trusted);
    let sizes = chunk_sizes(components.len(), phases);
    // Reserved before anything is named: a directory called `next` or `phase_2`
    // would otherwise take an id the tree's own scaffolding needs.
    used.insert("next".to_string());
    for i in 1..=phases {
        used.insert(format!("phase_{i}"));
    }

    let mut offset = 0;
    let mut previous_marker: Option<String> = None;
    let mut phase_ids: Vec<String> = Vec::new();
    for (phase_index, size) in sizes.iter().enumerate() {
        let group = &components[offset..offset + size];
        offset += size;
        if group.is_empty() {
            continue;
        }
        let mut group_ids = Vec::new();
        for component in group {
            let id = unique_id(&component.path, &mut used);
            let mut part_ids: HashSet<String> = HashSet::new();
            let parts: Vec<PartSpec> = component
                .children
                .iter()
                .map(|(child, _)| {
                    let relative = child
                        .strip_prefix(&component.path)
                        .unwrap_or(child)
                        .trim_start_matches('/');
                    PartSpec {
                        id: unique_id(relative, &mut part_ids),
                        title: relative.to_string(),
                        xp: DEFAULT_PART_XP,
                        evidence: Evidence {
                            paths: vec![glob_under(child)],
                            ..Default::default()
                        },
                    }
                })
                .collect();
            let parts = if parts.len() < 2 {
                // One sub-directory is not a breakdown — it is the component
                // with extra steps. Cover the whole thing in one part instead.
                vec![PartSpec {
                    id: "all".to_string(),
                    title: component.path.clone(),
                    xp: DEFAULT_PART_XP,
                    evidence: Evidence {
                        paths: vec![glob_under(&component.path)],
                        ..Default::default()
                    },
                }]
            } else {
                parts
            };

            meta.line(
                &id,
                format!(
                    "{} — {} commits, {} files, first touched {}, last {}",
                    component.path,
                    component.commits,
                    component.files,
                    ymd(component.first_unix),
                    ymd(component.last_unix),
                ),
            );
            nodes.push(NodeSpec {
                id: id.clone(),
                title: component
                    .path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&component.path)
                    .to_string(),
                summary: None,
                requires: previous_marker.iter().cloned().collect(),
                paths: vec![glob_under(&component.path)],
                parts,
            });
            group_ids.push(id);
        }

        if phases > 1 {
            let id = format!("phase_{}", phase_index + 1);
            let span_from = group.iter().map(|c| c.first_unix).min().unwrap_or(0);
            let span_to = group.iter().map(|c| c.first_unix).max().unwrap_or(0);
            let title = if ym(span_from) == ym(span_to) {
                format!("Phase {} · {}", phase_index + 1, ym(span_from))
            } else {
                format!(
                    "Phase {} · {} → {}",
                    phase_index + 1,
                    ym(span_from),
                    ym(span_to)
                )
            };
            meta.banner.insert(
                group_ids.first().cloned().unwrap_or_default(),
                format!(
                    "Phase {} — the components that first appeared {}. The grouping is the \
                     repository's own chronology, not a judgement: rename it.",
                    phase_index + 1,
                    if ym(span_from) == ym(span_to) {
                        format!("in {}", ym(span_from))
                    } else {
                        format!("between {} and {}", ym(span_from), ym(span_to))
                    }
                ),
            );
            nodes.push(NodeSpec {
                id: id.clone(),
                title,
                summary: None,
                requires: group_ids,
                paths: Vec::new(),
                parts: Vec::new(),
            });
            meta.line(
                &id,
                "A marker: no parts, so it closes as soon as everything it requires does."
                    .to_string(),
            );
            previous_marker = Some(id.clone());
            phase_ids.push(id);
        } else {
            previous_marker = None;
        }
    }

    let frontier_requires = if let Some(last) = phase_ids.last() {
        vec![last.clone()]
    } else {
        nodes.iter().map(|n| n.id.clone()).collect()
    };
    let frontier = frontier_node(frontier_requires);
    meta.banner.insert(
        frontier.id.clone(),
        "The frontier — everything above is already done, because history can only describe what \
         happened. This is the part you write."
            .to_string(),
    );
    meta.line(
        &frontier.id,
        "Its rule matches a tag no commit carries, so the milestone stays OPEN until you replace \
         it with real evidence — which is the only reason this draft does not read as a finished \
         quarter. (Tagging a commit `[progression:next]` closes it, if the work is done and the \
         rule never got written.)"
            .to_string(),
    );
    nodes.push(frontier);

    let spec = ProgressionSpec {
        version: 1,
        title: opts.title.clone(),
        season: opts.season.clone(),
        pr_pattern: marker.pattern.clone(),
        levels: Vec::new(),
        since_days: opts.since_days,
        nodes,
    };
    (spec, meta)
}

/// The starter plan for a repository with no history to mine.
///
/// Conventional locations rather than discovered ones, and worth having for a
/// reason the mined tree cannot claim: on a young repository it resolves near
/// zero and **fills in as the work lands**, which is the shape a progression
/// tree is supposed to have and the one a retro-fitted plan never gets back.
fn skeleton_spec(opts: &InitOptions, marker: &PrMarker) -> (ProgressionSpec, Meta) {
    let mut meta = Meta::default();
    let source_globs = vec![
        "src/**".to_string(),
        "lib/**".to_string(),
        "app/**".to_string(),
    ];
    let test_globs = vec!["tests/**".to_string(), "test/**".to_string()];

    let mut hardening_parts = vec![PartSpec {
        id: "docs".to_string(),
        title: "Someone else can run it from the README".to_string(),
        xp: 15,
        evidence: Evidence {
            paths: vec!["docs/**".to_string(), "README.md".to_string()],
            ..Default::default()
        },
    }];
    if marker.found {
        hardening_parts.push(PartSpec {
            id: "review".to_string(),
            title: "Changes land through review".to_string(),
            xp: 15,
            evidence: Evidence {
                paths: source_globs.clone(),
                commits: Some(0),
                prs: Some(3),
                ..Default::default()
            },
        });
    } else {
        hardening_parts.push(PartSpec {
            id: "coverage".to_string(),
            title: "The tests are exercised, not written once".to_string(),
            xp: 15,
            evidence: Evidence {
                paths: test_globs.clone(),
                commits: Some(5),
                ..Default::default()
            },
        });
    }

    let nodes = vec![
        NodeSpec {
            id: "foundations".to_string(),
            title: "Foundations".to_string(),
            summary: Some("It builds, it is configured, and CI says so.".to_string()),
            requires: Vec::new(),
            paths: Vec::new(),
            parts: vec![
                PartSpec {
                    id: "skeleton".to_string(),
                    title: "The source tree exists".to_string(),
                    xp: DEFAULT_PART_XP,
                    evidence: Evidence {
                        paths: source_globs.clone(),
                        ..Default::default()
                    },
                },
                PartSpec {
                    id: "manifest".to_string(),
                    title: "Dependencies are declared".to_string(),
                    xp: DEFAULT_PART_XP,
                    evidence: Evidence {
                        paths: vec![
                            "Cargo.toml".to_string(),
                            "pyproject.toml".to_string(),
                            "package.json".to_string(),
                            "go.mod".to_string(),
                        ],
                        ..Default::default()
                    },
                },
                PartSpec {
                    id: "ci".to_string(),
                    title: "CI runs on every push".to_string(),
                    xp: 15,
                    evidence: Evidence {
                        paths: vec![".github/workflows/**".to_string()],
                        ..Default::default()
                    },
                },
            ],
        },
        NodeSpec {
            id: "core".to_string(),
            title: "The core path".to_string(),
            summary: Some("The thing this repository is for, end to end.".to_string()),
            requires: vec!["foundations".to_string()],
            paths: Vec::new(),
            parts: vec![
                PartSpec {
                    id: "engine".to_string(),
                    title: "The main path is built out".to_string(),
                    xp: 30,
                    evidence: Evidence {
                        paths: source_globs.clone(),
                        commits: Some(10),
                        ..Default::default()
                    },
                },
                PartSpec {
                    id: "tests".to_string(),
                    title: "It is covered by tests".to_string(),
                    xp: 20,
                    evidence: Evidence {
                        paths: test_globs,
                        ..Default::default()
                    },
                },
            ],
        },
        NodeSpec {
            id: "hardening".to_string(),
            title: "Hardening".to_string(),
            summary: Some("The work that makes it somebody else's to run.".to_string()),
            requires: vec!["core".to_string()],
            paths: Vec::new(),
            parts: hardening_parts,
        },
        NodeSpec {
            id: "ship".to_string(),
            title: "Ship it".to_string(),
            summary: Some("A marker: no parts, so it closes when the rest does.".to_string()),
            requires: vec!["hardening".to_string()],
            paths: Vec::new(),
            parts: Vec::new(),
        },
    ];

    meta.banner.insert(
        "foundations".to_string(),
        "A starter plan, not a mined one: these globs are conventional locations. Point them at \
         your layout, then delete what does not apply."
            .to_string(),
    );
    meta.line(
        "core",
        "`commits: 10` is a goal, not a measurement — the one number here nothing discovered."
            .to_string(),
    );
    meta.line(
        "ship",
        "No parts: a marker milestone, done as soon as everything it requires is.".to_string(),
    );

    let spec = ProgressionSpec {
        version: 1,
        title: opts.title.clone(),
        season: opts.season.clone(),
        pr_pattern: marker.pattern.clone(),
        levels: Vec::new(),
        since_days: opts.since_days,
        nodes,
    };
    (spec, meta)
}

/// Double-quoted YAML scalar.
fn dq(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Single-quoted YAML scalar — no escape processing, so a regex stays readable.
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `[a, b]` for identifiers.
fn flow_ids(ids: &[String]) -> String {
    format!("[{}]", ids.join(", "))
}

/// `["a/**", "b"]` for globs.
fn flow_globs(globs: &[String]) -> String {
    let inner: Vec<String> = globs.iter().map(|g| dq(g)).collect();
    format!("[{}]", inner.join(", "))
}

/// Wrap `text` into comment lines at `indent`, breaking on spaces.
fn comment(text: &str, indent: &str, out: &mut String) {
    const WIDTH: usize = 92;
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            out.push_str(indent);
            out.push_str("#\n");
            continue;
        }
        if paragraph.starts_with(' ') {
            // A line that indents itself is preformatted — a command someone is
            // meant to copy. Re-wrapping it would break it in half.
            out.push_str(indent);
            out.push_str("# ");
            out.push_str(paragraph.trim_end());
            out.push('\n');
            continue;
        }
        // A numbered or bulleted item hangs its continuation lines, so a wrapped
        // step does not read as the next one.
        let hang = paragraph.starts_with("- ")
            || paragraph.split_once(". ").is_some_and(|(head, _)| {
                head.len() <= 2 && head.chars().all(|c| c.is_ascii_digit())
            });
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if !line.is_empty() && indent.len() + 2 + line.len() + 1 + word.len() > WIDTH {
                out.push_str(indent);
                out.push_str("# ");
                out.push_str(&line);
                out.push('\n');
                line.clear();
                if hang {
                    line.push_str("   ");
                }
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.trim().is_empty() {
            out.push_str(indent);
            out.push_str("# ");
            out.push_str(&line);
            out.push('\n');
        }
    }
}

/// The draft as commented YAML.
///
/// Hand-written rather than `serde_yaml::to_string`, because the comments are
/// most of what makes this a draft rather than a file: a scaffold that does not
/// say which parts were guessed is one a reader has to take on faith. The
/// output is parsed back and validated by [`draft`] before it reaches disk.
fn render_yaml(
    spec: &ProgressionSpec,
    meta: &Meta,
    source: &DraftSource,
    marker: &PrMarker,
    opts: &InitOptions,
    now_unix: u64,
) -> String {
    let mut out = String::new();

    comment(
        &format!(
            "Progression plan — a DRAFT scaffolded by `slop-gate progression init` on {}.",
            ymd(now_unix)
        ),
        "",
        &mut out,
    );
    comment("", "", &mut out);
    match source {
        DraftSource::Mined {
            components,
            commits,
        } => {
            comment(
                &format!(
                    "Mined {components} component(s) from {commits} commit(s). Every milestone \
                     above the frontier describes work that has ALREADY LANDED — history can say \
                     what a repository has done, never what it meant to do — so this file will \
                     resolve close to 100% the first time you run it. That is not a bug in the \
                     scaffold and it is not something to paper over with a sliding window: the \
                     done part is your tree's roots, and the milestone at the bottom is the one \
                     only you can write."
                ),
                "",
                &mut out,
            );
        }
        DraftSource::Skeleton { reason } => {
            comment(
                &format!(
                    "No tree was mined ({reason}), so this is a starter plan: the globs point at \
                     conventional locations rather than discovered ones. On a young repository \
                     that is the better shape anyway — it resolves near zero today and fills in \
                     as the work lands, which is exactly what a plan retro-fitted to a mature \
                     repository can never do."
                ),
                "",
                &mut out,
            );
        }
    }
    comment("", "", &mut out);
    if marker.found {
        comment(
            &format!(
                "PR marker: {} matched {}% of the commits walked, so `prs:` thresholds will \
                 count. Evidence below counts commits; swap in `prs:` where a milestone is \
                 better measured in merged changes than in pushes.",
                marker.label, marker.coverage_pct
            ),
            "",
            &mut out,
        );
    } else {
        comment(
            "No PR marker was found in any commit subject, so `prs:` thresholds would never \
             close and every rule below counts commits instead. If this repository does mark \
             merges, set `pr_pattern` to the regex that captures the number.",
            "",
            &mut out,
        );
    }
    if opts.since_days.is_some() {
        comment("", "", &mut out);
        comment(
            "`since_days` makes this a SLIDING window: a milestone closed by commits that later \
             fall out the back of it re-opens on its own. Useful for a sprint tree that is meant \
             to reset; wrong for a plan you expect to stay closed. Delete the line to resolve \
             against the whole history.",
            "",
            &mut out,
        );
    }
    comment("", "", &mut out);
    comment(
        "Before you commit this:\n\
         1. Rename the milestones and parts to the WORK, not the directory — a tree that reads \
         `src` teaches a newcomer nothing.\n\
         2. Delete what is not part of the plan. A scaffolded milestone nobody meant is worse \
         than a missing one.\n\
         3. Replace the frontier milestone with what you are actually planning.\n\
         4. Then draw it:\n\
         \u{20} slop-gate progression --spec progression.yaml --svg docs/progression.svg --readme README.md",
        "",
        &mut out,
    );
    out.push('\n');

    out.push_str(&format!("version: {}\n", spec.version));
    out.push_str(&format!("title: {}\n", dq(&spec.title)));
    if let Some(season) = &spec.season {
        out.push_str(&format!("season: {}\n", dq(season)));
    }
    out.push_str(&format!("pr_pattern: {}\n", sq(&spec.pr_pattern)));
    if let Some(days) = spec.since_days {
        out.push_str(&format!("since_days: {days}\n"));
    }
    out.push('\n');
    out.push_str("nodes:\n");

    for node in &spec.nodes {
        if let Some(banner) = meta.banner.get(&node.id) {
            out.push('\n');
            out.push_str("  # ");
            out.push_str(&"─".repeat(60));
            out.push('\n');
            comment(banner, "  ", &mut out);
        }
        for line in meta.lines.get(&node.id).into_iter().flatten() {
            comment(line, "  ", &mut out);
        }
        out.push_str(&format!("  - id: {}\n", node.id));
        out.push_str(&format!("    title: {}\n", dq(&node.title)));
        if let Some(summary) = &node.summary {
            out.push_str(&format!("    summary: {}\n", dq(summary)));
        }
        if !node.requires.is_empty() {
            out.push_str(&format!("    requires: {}\n", flow_ids(&node.requires)));
        }
        if !node.paths.is_empty() {
            out.push_str(&format!("    paths: {}\n", flow_globs(&node.paths)));
        }
        if node.parts.is_empty() {
            continue;
        }
        out.push_str("    parts:\n");
        for part in &node.parts {
            out.push_str(&format!("      - id: {}\n", part.id));
            out.push_str(&format!("        title: {}\n", dq(&part.title)));
            out.push_str(&format!("        xp: {}\n", part.xp));
            out.push_str(&format!(
                "        evidence: {}\n",
                flow_evidence(&part.evidence)
            ));
        }
    }
    out
}

/// `{ paths: ["a/**"], commits: 2 }` — only the keys that are set.
fn flow_evidence(ev: &Evidence) -> String {
    let mut fields: Vec<String> = Vec::new();
    if !ev.paths.is_empty() {
        fields.push(format!("paths: {}", flow_globs(&ev.paths)));
    }
    if let Some(subject) = &ev.subject {
        fields.push(format!("subject: {}", sq(subject)));
    }
    if !ev.authors.is_empty() {
        fields.push(format!("authors: {}", flow_globs(&ev.authors)));
    }
    if let Some(commits) = ev.commits {
        fields.push(format!("commits: {commits}"));
    }
    if let Some(prs) = ev.prs {
        fields.push(format!("prs: {prs}"));
    }
    if ev.manual {
        fields.push("manual: true".to_string());
    }
    if fields.is_empty() {
        // Unreachable for anything this module builds, and `validate()` rejects
        // it — a part that selects nothing is a plan item nobody wired up. Emit
        // it rather than panicking so the round-trip check reports it.
        return "{}".to_string();
    }
    format!("{{ {} }}", fields.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progression::resolve;

    const DAY: u64 = 86_400;
    /// 2024-01-01T00:00:00Z, so the emitted dates are recognisable in failures.
    const BASE: u64 = 1_704_067_200;

    fn commit(sha: &str, subject: &str, paths: &[&str], ts: u64) -> Commit {
        Commit {
            sha: sha.to_string(),
            subject: subject.to_string(),
            message: subject.to_string(),
            author: "dev".to_string(),
            timestamp_unix: ts,
            paths: paths.iter().map(|p| p.to_string()).collect(),
            pr: None,
        }
    }

    /// Six services, each five commits, each first touched a month apart.
    fn services() -> Vec<Commit> {
        let names = ["auth", "payments", "ingest", "api", "ui", "ops"];
        let mut out = Vec::new();
        for (i, name) in names.iter().enumerate() {
            for k in 0..5u64 {
                out.push(commit(
                    &format!("{name}{k}"),
                    &format!("feat({name}): step {k} (#{})", 100 + i * 10 + k as usize),
                    &[
                        &format!("services/{name}/src/lib.rs"),
                        &format!("services/{name}/docs/notes.md"),
                    ],
                    BASE + DAY * (i as u64 * 30 + k),
                ));
            }
        }
        out
    }

    fn opts() -> InitOptions {
        InitOptions {
            title: "Test plan".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn dir_prefix_walks_directories_and_stops_before_the_file() {
        assert_eq!(dir_prefix("a/b/c.rs", 1), Some("a"));
        assert_eq!(dir_prefix("a/b/c.rs", 2), Some("a/b"));
        assert_eq!(dir_prefix("a/b/c.rs", 3), None);
        assert_eq!(dir_prefix("README.md", 1), None);
    }

    #[test]
    fn a_container_directory_splits_into_its_children() {
        let components = discover(&services(), &opts());
        let paths: Vec<&str> = components.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "services/auth",
                "services/payments",
                "services/ingest",
                "services/api",
                "services/ui",
                "services/ops"
            ],
            "`services` holds six separable components, so naming it once says nothing"
        );
    }

    #[test]
    fn a_wrapper_directory_is_descended_through() {
        // Everything lives under `repo/main/...`: the two outer levels carry no
        // information, and stopping at them would give every milestone one glob.
        let mut commits = Vec::new();
        for (i, name) in ["parser", "printer"].iter().enumerate() {
            for k in 0..4u64 {
                commits.push(commit(
                    &format!("{name}{k}"),
                    "work",
                    &[&format!("repo/main/{name}/mod.rs")],
                    BASE + DAY * (i as u64 * 10 + k),
                ));
            }
        }
        let components = discover(&commits, &InitOptions { depth: 4, ..opts() });
        let paths: Vec<&str> = components.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, vec!["repo/main/parser", "repo/main/printer"]);
    }

    #[test]
    fn build_output_and_lockfiles_are_never_components() {
        let mut commits = Vec::new();
        for k in 0..9u64 {
            commits.push(commit(
                &format!("c{k}"),
                "build",
                &[
                    "target/debug/thing",
                    "node_modules/left-pad/index.js",
                    "Cargo.lock",
                    "engine/src/lib.rs",
                ],
                BASE + DAY * k,
            ));
        }
        let components = discover(&commits, &opts());
        let paths: Vec<&str> = components.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, vec!["engine"]);
    }

    #[test]
    fn a_prefix_counts_once_per_commit_however_many_files_it_touched() {
        // One sweeping commit over 50 files must not outweigh a directory that
        // took ten real ones.
        let sweep: Vec<&str> = (0..50)
            .map(|_| "sweep/generated/file.rs")
            .collect::<Vec<_>>();
        let commits = vec![commit("sweep", "format everything", &sweep, BASE)];
        let tallies = tally(&commits, 2);
        assert_eq!(tallies["sweep"].commits, 1);
    }

    #[test]
    fn the_marker_is_the_most_specific_pattern_that_matches() {
        // Both `\(#\d+\)` and the bare `#\d+` match these subjects. Coverage
        // alone would pick the loose one, which also matches issue references.
        let commits: Vec<Commit> = (0..10)
            .map(|i| commit(&format!("c{i}"), &format!("thing: do it (#{i})"), &[], BASE))
            .collect();
        let marker = detect_pr_marker(&commits);
        assert!(marker.found);
        assert_eq!(marker.pattern, r"\(#(\d+)\)");
        assert_eq!(marker.coverage_pct, 100);
    }

    #[test]
    fn a_repo_that_marks_nothing_keeps_the_default_and_says_so() {
        let commits: Vec<Commit> = (0..10)
            .map(|i| commit(&format!("c{i}"), "just a message", &[], BASE))
            .collect();
        let marker = detect_pr_marker(&commits);
        assert!(!marker.found);
        assert_eq!(marker.pattern, crate::progression::spec::DEFAULT_PR_PATTERN);
    }

    #[test]
    fn a_gitlab_repo_is_read_as_gitlab() {
        let commits: Vec<Commit> = (0..10)
            .map(|i| {
                commit(
                    &format!("c{i}"),
                    &format!("thing\n\nSee merge request group/project!{i}"),
                    &[],
                    BASE,
                )
            })
            .collect();
        // `detect_pr_marker` reads subjects; put the trailer there too.
        let commits: Vec<Commit> = commits
            .into_iter()
            .map(|mut c| {
                c.subject = c.message.replace('\n', " ");
                c
            })
            .collect();
        let marker = detect_pr_marker(&commits);
        assert!(marker.found);
        assert!(marker.pattern.contains("merge request"));
    }

    #[test]
    fn the_mined_draft_parses_back_and_validates() {
        let history = services();
        let draft = draft(&history, &WalkStats::default(), &opts(), BASE + DAY * 400).unwrap();
        assert!(matches!(draft.source, DraftSource::Mined { .. }));
        // 6 components + 3 phase markers + the frontier.
        assert_eq!(draft.spec.nodes.len(), 10);
        assert_eq!(
            draft
                .spec
                .nodes
                .iter()
                .filter(|n| n.parts.is_empty())
                .count(),
            3
        );
        // `draft()` already validated the re-parsed spec; assert the YAML is
        // what was validated, not a second rendering.
        let reparsed: ProgressionSpec = serde_yaml::from_str(&draft.yaml).unwrap();
        reparsed.validate().unwrap();
        assert_eq!(reparsed.title, "Test plan");
        assert_eq!(reparsed.pr_pattern, r"\(#(\d+)\)");
    }

    #[test]
    fn the_frontier_is_the_only_milestone_left_open() {
        // The point of the whole scaffold: everything mined is already done —
        // it describes commits that landed — and exactly one milestone is the
        // author's to write. A draft that resolved to 100% would have nothing
        // to say on day one.
        let history = services();
        let draft = draft(&history, &WalkStats::default(), &opts(), BASE + DAY * 400).unwrap();
        let snap = resolve::resolve(&draft.spec, &history, "deadbeef", BASE + DAY * 400).unwrap();

        let frontier = snap
            .node(
                draft
                    .frontier_id
                    .as_deref()
                    .expect("a mined draft has a frontier"),
            )
            .expect("frontier resolved");
        assert_eq!(frontier.state, resolve::NodeState::Available);
        assert_eq!(frontier.xp_earned, 0);
        assert_eq!(
            snap.totals.nodes_done,
            snap.totals.nodes_total - 1,
            "everything but the frontier describes work that already landed"
        );
    }

    #[test]
    fn ordinary_prose_cannot_close_the_frontier() {
        // Found by running this against a 2,760-commit repository: the first
        // version of the placeholder matched the word `REPLACE-ME`, one commit
        // body used it in passing, and the frontier scaffolded itself shut.
        let mut history = services();
        history.push(commit(
            "prose",
            "fix: replace-me markers in the template, and REPLACE ME notes in the docs",
            &["services/auth/src/lib.rs"],
            BASE + DAY * 200,
        ));
        let draft = draft(&history, &WalkStats::default(), &opts(), BASE + DAY * 400).unwrap();
        let snap = resolve::resolve(&draft.spec, &history, "deadbeef", BASE + DAY * 400).unwrap();
        let frontier = snap
            .node(draft.frontier_id.as_deref().unwrap())
            .expect("frontier resolved");
        assert_eq!(frontier.state, resolve::NodeState::Available);
    }

    #[test]
    fn a_windowed_walk_gets_a_flat_tree_because_first_seen_means_nothing_in_a_window() {
        let history = services();
        let windowed = InitOptions {
            since_days: Some(30),
            ..opts()
        };
        let draft = draft(&history, &WalkStats::default(), &windowed, BASE + DAY * 400).unwrap();
        assert!(!draft.spec.nodes.iter().any(|n| n.id.starts_with("phase_")));
        assert_eq!(draft.spec.since_days, Some(30));
        assert!(draft.yaml.contains("SLIDING window"));
        // The frontier still exists, and now depends on every component.
        let frontier = draft.spec.nodes.last().unwrap();
        assert_eq!(frontier.id, "next");
        assert_eq!(frontier.requires.len(), 6);
    }

    #[test]
    fn a_truncated_walk_is_treated_the_same_way() {
        let stats = WalkStats {
            truncated: true,
            ..Default::default()
        };
        let draft = draft(&services(), &stats, &opts(), BASE + DAY * 400).unwrap();
        assert!(!draft.spec.nodes.iter().any(|n| n.id.starts_with("phase_")));
        assert!(draft
            .notes
            .iter()
            .any(|n| n.contains("flat rather than phased")));
    }

    #[test]
    fn too_little_history_gets_the_starter_skeleton() {
        let commits = vec![
            commit("a", "init", &["src/main.rs"], BASE),
            commit("b", "more", &["src/main.rs"], BASE + DAY),
        ];
        let draft = draft(&commits, &WalkStats::default(), &opts(), BASE + DAY * 2).unwrap();
        assert!(matches!(draft.source, DraftSource::Skeleton { .. }));
        let ids: Vec<&str> = draft.spec.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["foundations", "core", "hardening", "ship"]);
        serde_yaml::from_str::<ProgressionSpec>(&draft.yaml)
            .unwrap()
            .validate()
            .unwrap();
    }

    #[test]
    fn an_empty_repository_still_produces_a_usable_plan() {
        let draft = draft(&[], &WalkStats::default(), &opts(), BASE).unwrap();
        assert!(matches!(draft.source, DraftSource::Skeleton { .. }));
        let snap = resolve::resolve(&draft.spec, &[], "0", BASE).unwrap();
        assert_eq!(snap.totals.nodes_done, 0, "a fresh plan starts at zero");
        assert!(snap.totals.nodes_total >= 4);
    }

    #[test]
    fn the_skeleton_counts_prs_only_where_the_repo_marks_them() {
        let marked = skeleton_spec(
            &opts(),
            &PrMarker {
                pattern: r"\(#(\d+)\)".to_string(),
                label: "x".to_string(),
                coverage_pct: 90,
                found: true,
            },
        )
        .0;
        let hardening = marked.nodes.iter().find(|n| n.id == "hardening").unwrap();
        assert!(hardening.parts.iter().any(|p| p.evidence.prs == Some(3)));

        let unmarked = skeleton_spec(
            &opts(),
            &PrMarker {
                pattern: r"\(#(\d+)\)".to_string(),
                label: "none".to_string(),
                coverage_pct: 0,
                found: false,
            },
        )
        .0;
        let hardening = unmarked.nodes.iter().find(|n| n.id == "hardening").unwrap();
        assert!(
            hardening.parts.iter().all(|p| p.evidence.prs.is_none()),
            "a `prs:` threshold in a repo with no marker never closes"
        );
    }

    #[test]
    fn components_with_the_same_leaf_name_get_distinct_ids() {
        let mut commits = Vec::new();
        for (i, top) in ["alpha", "beta"].iter().enumerate() {
            for k in 0..4u64 {
                commits.push(commit(
                    &format!("{top}{k}"),
                    "work",
                    &[&format!("{top}/gateway/lib.rs")],
                    BASE + DAY * (i as u64 * 10 + k),
                ));
            }
        }
        let draft = draft(&commits, &WalkStats::default(), &opts(), BASE + DAY * 40).unwrap();
        let ids: Vec<&str> = draft
            .spec
            .nodes
            .iter()
            .filter(|n| n.id != "next")
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(ids, vec!["gateway", "beta_gateway"]);
    }

    #[test]
    fn the_regex_survives_the_yaml_round_trip() {
        // A backslash-heavy scalar is the one thing a hand-written emitter gets
        // wrong, and the failure is silent: `prs:` thresholds stop closing.
        let spec = ProgressionSpec {
            version: 1,
            title: "T \"quoted\" \\ backslash".to_string(),
            season: None,
            pr_pattern: r"See merge request [^\s]*!(\d+)".to_string(),
            levels: Vec::new(),
            since_days: None,
            nodes: vec![NodeSpec {
                id: "n".to_string(),
                title: "n".to_string(),
                summary: None,
                requires: Vec::new(),
                paths: Vec::new(),
                parts: vec![PartSpec {
                    id: "p".to_string(),
                    title: "p".to_string(),
                    xp: 10,
                    evidence: Evidence {
                        paths: vec!["a/**/*.rs".to_string()],
                        subject: Some(r"^fix\(.*\):".to_string()),
                        ..Default::default()
                    },
                }],
            }],
        };
        let yaml = render_yaml(
            &spec,
            &Meta::default(),
            &DraftSource::Skeleton {
                reason: "test".to_string(),
            },
            &PrMarker {
                pattern: spec.pr_pattern.clone(),
                label: "gitlab".to_string(),
                coverage_pct: 50,
                found: true,
            },
            &opts(),
            BASE,
        );
        let back: ProgressionSpec = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.pr_pattern, spec.pr_pattern);
        assert_eq!(back.title, spec.title);
        assert_eq!(
            back.nodes[0].parts[0].evidence.subject.as_deref(),
            Some(r"^fix\(.*\):")
        );
        assert_eq!(back.nodes[0].parts[0].evidence.paths, vec!["a/**/*.rs"]);
    }

    #[test]
    fn a_copyable_command_is_not_re_wrapped() {
        let mut out = String::new();
        comment(
            "do this:\n  slop-gate progression --spec a.yaml --svg b.svg --readme R.md",
            "",
            &mut out,
        );
        assert!(out.contains("#   slop-gate progression --spec a.yaml --svg b.svg --readme R.md\n"));
    }

    #[test]
    fn chunk_sizes_spread_the_remainder() {
        assert_eq!(chunk_sizes(6, 3), vec![2, 2, 2]);
        assert_eq!(chunk_sizes(7, 3), vec![3, 2, 2]);
        assert_eq!(chunk_sizes(4, 2), vec![2, 2]);
        assert_eq!(chunk_sizes(5, 1), vec![5]);
    }

    #[test]
    fn dates_are_the_dates_they_claim_to_be() {
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(1_700_000_000), "2023-11-14");
        assert_eq!(ym(BASE), "2024-01");
        // A leap day, the one the arithmetic gets wrong when it is wrong.
        assert_eq!(ymd(1_709_164_800), "2024-02-29");
    }

    #[test]
    fn identifiers_are_safe_to_use_as_anchors() {
        assert_eq!(ident("service_55"), "service_55");
        assert_eq!(ident(".github"), "github");
        assert_eq!(ident("my-lib"), "my_lib");
        assert_eq!(ident("2fa"), "n_2fa");
        assert_eq!(ident("!!!"), "node");
    }
}
