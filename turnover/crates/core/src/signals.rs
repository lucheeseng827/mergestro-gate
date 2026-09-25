//! The per-commit classifier. Input: every changed file with both sides of its text and a
//! significance mask per side. Output: how many significant added lines fall into each
//! bucket. Nothing here knows about git; the walker hands it strings.
//!
//! ## The buckets, precisely
//!
//! A line is *eligible* when its side's mask says it is significant, its normalised form is
//! at least `min_line_chars` long and it is not an import statement. Only eligible lines can
//! be moved, copy/pasted or in a duplicated block; `added` and `deleted` count every
//! significant line regardless, because they are the denominators.
//!
//! * **moved** — an eligible added line whose normalised text matches an eligible deleted
//!   line *in the same commit* (any file). Matching is 1:1 on a multiset, so five deletions
//!   can satisfy at most five additions. This is refactoring's fingerprint: code that left one
//!   place and arrived in another.
//! * **copy_pasted** — an eligible added line, not moved, whose normalised text occurs at
//!   least twice in the post-change content of the commit's touched files. The second
//!   occurrence may be another added line (paste twice) or pre-existing text (paste from).
//! * **dup_block** — an eligible added line covered by a run of `block_min_lines`
//!   consecutive significant lines that occurs at two different positions in the post-change
//!   content of the touched files (non-overlapping, or in different files). The block signal
//!   is what copy/paste looks like when it is a whole function. By default the run is
//!   compared *rename-insensitively* ([`CloneType::Renamed`]): lines are abstracted to
//!   their shape first ([`crate::tokens`]), so a pasted function with its identifiers and
//!   literals changed still matches, and a run must carry at least `block_min_tokens`
//!   tokens so that trivially repetitive shapes (`x = 1;` chains) never count.
//!
//! **Scope caveat, stated once:** duplicates are searched in the files the commit touched,
//! not the whole tree. That under-counts pastes from untouched files and is a v0 choice made
//! for the baseline's sake (a whole-tree line index per commit is the v1 walker). It never
//! over-counts, which is the direction a gate must err in.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};

use crate::attribution::Origin;
use crate::language::Language;
use crate::line::{is_import, normalize, Mask};

/// Tunables for the classifier. Defaults are the ones the documentation describes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SignalConfig {
    /// Minimum normalised length for a line to be matchable (moved / pasted / block member).
    pub min_line_chars: usize,
    /// Consecutive significant lines that make a duplicated block.
    pub block_min_lines: usize,
    /// Exclude import-style lines from matching.
    pub ignore_imports: bool,
    /// How duplicated blocks are matched: verbatim, or with identifiers and literals
    /// abstracted so a paste-then-rename still matches.
    pub clone_type: CloneType,
    /// Minimum tokens in an abstracted block before it can count as a duplicate — the guard
    /// against structural shapes that repeat everywhere.
    pub block_min_tokens: usize,
}

impl Default for SignalConfig {
    fn default() -> Self {
        SignalConfig {
            min_line_chars: 10,
            block_min_lines: 6,
            ignore_imports: true,
            clone_type: CloneType::Renamed,
            block_min_tokens: 24,
        }
    }
}

/// Block-duplication matching mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloneType {
    /// Type-1: the lines are identical after whitespace normalisation.
    Exact,
    /// Type-2: identical after identifiers and literals are abstracted (default).
    Renamed,
}

/// One changed file. `old`/`new` are `None` for additions/deletions respectively; each mask
/// must have exactly as many entries as its text has lines (`line::line_count`).
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: String,
    pub language: Language,
    pub old: Option<String>,
    pub new: Option<String>,
    pub old_mask: Option<Mask>,
    pub new_mask: Option<Mask>,
}

/// One commit, as the walker delivers it.
#[derive(Debug, Clone)]
pub struct CommitInput {
    pub sha: String,
    pub parent: Option<String>,
    pub timestamp_unix: i64,
    pub author: String,
    pub is_merge: bool,
    /// AI-coauthored or human, from the commit's metadata (see [`crate::attribution`]).
    pub origin: Origin,
    pub files: Vec<FileChange>,
}

/// Bucket counts. `churned` is filled by [`crate::churn::attribute`], not by [`classify`],
/// because it needs later commits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Counts {
    pub added: u64,
    pub deleted: u64,
    pub moved: u64,
    pub copy_pasted: u64,
    pub dup_block: u64,
    pub churned: u64,
}

impl Counts {
    pub fn add(&mut self, other: &Counts) {
        self.added += other.added;
        self.deleted += other.deleted;
        self.moved += other.moved;
        self.copy_pasted += other.copy_pasted;
        self.dup_block += other.dup_block;
        self.churned += other.churned;
    }
}

/// The classified commit — one row of the baseline file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommitSignals {
    pub sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub timestamp_unix: i64,
    pub author: String,
    #[serde(default)]
    pub is_merge: bool,
    /// AI-coauthored or human. Rows written before attribution existed read as human.
    #[serde(default)]
    pub origin: Origin,
    #[serde(default)]
    pub files: u32,
    pub counts: Counts,
    #[serde(default)]
    pub by_language: BTreeMap<String, Counts>,
    /// Content fingerprints of eligible added lines, per path — churn attribution input.
    /// Never serialised: it is walk-time state, not a baseline fact.
    #[serde(skip)]
    pub added_fingerprints: Vec<(String, u64)>,
    /// Fingerprints of eligible deleted lines, per path — the other half of churn.
    #[serde(skip)]
    pub deleted_fingerprints: Vec<(String, u64)>,
}

/// A per-file explanation of one commit's classification — what `turnover explain` prints.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileExplanation {
    pub path: String,
    pub language: String,
    pub counts: Counts,
    /// 1-based line numbers (new side) per bucket.
    pub moved_lines: Vec<usize>,
    pub copy_pasted_lines: Vec<usize>,
    pub dup_block_lines: Vec<usize>,
    /// For each duplicated block in this file: where it starts and where its twin is.
    #[serde(default)]
    pub dup_blocks: Vec<DupBlock>,
}

/// One duplicated block: `lines` consecutive significant lines starting at `line` (1-based,
/// new side) that also occur at `other_line` of `other_path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DupBlock {
    pub line: usize,
    pub lines: usize,
    pub other_path: String,
    pub other_line: usize,
}

/// One eligible or significant line on one side of one file.
struct Line<'a> {
    file: usize,
    /// 0-based index within its side.
    idx: usize,
    norm: String,
    eligible: bool,
    lang: Language,
    path: &'a str,
}

fn fingerprint(path: &str, norm: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    norm.hash(&mut h);
    h.finish()
}

fn eligible(cfg: &SignalConfig, lang: Language, norm: &str) -> bool {
    norm.chars().count() >= cfg.min_line_chars && !(cfg.ignore_imports && is_import(lang, norm))
}

/// Classify one commit. See the module docs for the bucket definitions.
pub fn classify(commit: &CommitInput, cfg: &SignalConfig) -> CommitSignals {
    let (signals, _) = classify_inner(commit, cfg, false);
    signals
}

/// [`classify`] plus a per-file explanation with line numbers.
pub fn explain(commit: &CommitInput, cfg: &SignalConfig) -> (CommitSignals, Vec<FileExplanation>) {
    classify_inner(commit, cfg, true)
}

fn classify_inner(
    commit: &CommitInput,
    cfg: &SignalConfig,
    want_explain: bool,
) -> (CommitSignals, Vec<FileExplanation>) {
    let mut out = CommitSignals {
        sha: commit.sha.clone(),
        parent: commit.parent.clone(),
        timestamp_unix: commit.timestamp_unix,
        author: commit.author.clone(),
        is_merge: commit.is_merge,
        origin: commit.origin,
        files: commit.files.len() as u32,
        ..Default::default()
    };
    let mut explanations: Vec<FileExplanation> = if want_explain {
        commit
            .files
            .iter()
            .map(|f| FileExplanation {
                path: f.path.clone(),
                language: f.language.as_str().to_string(),
                ..Default::default()
            })
            .collect()
    } else {
        Vec::new()
    };

    // 1. Diff every file, collecting significant added and deleted lines.
    let mut added: Vec<Line<'_>> = Vec::new();
    let mut deleted: Vec<Line<'_>> = Vec::new();
    // Post-change significant lines per file (all of them, not only added): the corpus that
    // copy/paste and duplicated-block lookups search.
    let mut new_side: Vec<Vec<(usize, String)>> = Vec::with_capacity(commit.files.len());

    for (fi, f) in commit.files.iter().enumerate() {
        let old = f.old.as_deref().unwrap_or("");
        let new = f.new.as_deref().unwrap_or("");
        let empty: Mask = Vec::new();
        let old_mask = f.old_mask.as_ref().unwrap_or(&empty);
        let new_mask = f.new_mask.as_ref().unwrap_or(&empty);

        let diff = TextDiff::from_lines(old, new);
        for change in diff.iter_all_changes() {
            match change.tag() {
                ChangeTag::Equal => {}
                ChangeTag::Insert => {
                    let idx = change.new_index().unwrap_or(0);
                    if new_mask.get(idx).copied().unwrap_or(false) {
                        let norm = normalize(change.value());
                        let el = eligible(cfg, f.language, &norm);
                        added.push(Line {
                            file: fi,
                            idx,
                            norm,
                            eligible: el,
                            lang: f.language,
                            path: &f.path,
                        });
                    }
                }
                ChangeTag::Delete => {
                    let idx = change.old_index().unwrap_or(0);
                    if old_mask.get(idx).copied().unwrap_or(false) {
                        let norm = normalize(change.value());
                        let el = eligible(cfg, f.language, &norm);
                        deleted.push(Line {
                            file: fi,
                            idx,
                            norm,
                            eligible: el,
                            lang: f.language,
                            path: &f.path,
                        });
                    }
                }
            }
        }

        let sig: Vec<(usize, String)> = new
            .split_inclusive('\n')
            .enumerate()
            .filter(|(i, _)| new_mask.get(*i).copied().unwrap_or(false))
            .map(|(i, raw)| (i, normalize(raw)))
            .collect();
        new_side.push(sig);
    }
    // The block-matching representation of every significant new-side line: the line itself
    // (Type-1) or its abstracted shape plus token count (Type-2).
    let shapes: Vec<Vec<(String, usize)>> = new_side
        .iter()
        .enumerate()
        .map(|(fi, file)| {
            let lang = commit.files[fi].language;
            file.iter()
                .map(|(_, norm)| match cfg.clone_type {
                    CloneType::Exact => (norm.clone(), usize::MAX),
                    CloneType::Renamed => crate::tokens::abstract_line(lang, norm),
                })
                .collect()
        })
        .collect();

    // 2. Moved: multiset match of eligible added lines against eligible deleted lines.
    let mut deleted_pool: HashMap<&str, u32> = HashMap::new();
    for d in deleted.iter().filter(|d| d.eligible) {
        *deleted_pool.entry(d.norm.as_str()).or_insert(0) += 1;
    }
    let mut moved = vec![false; added.len()];
    for (i, a) in added.iter().enumerate() {
        if !a.eligible {
            continue;
        }
        if let Some(n) = deleted_pool.get_mut(a.norm.as_str()) {
            if *n > 0 {
                *n -= 1;
                moved[i] = true;
            }
        }
    }

    // 3. Copy/paste: eligible, not moved, and its text occurs >= 2 times across the
    //    post-change touched files.
    let mut occurrences: HashMap<&str, u32> = HashMap::new();
    for file in &new_side {
        for (_, norm) in file {
            *occurrences.entry(norm.as_str()).or_insert(0) += 1;
        }
    }
    let mut pasted = vec![false; added.len()];
    for (i, a) in added.iter().enumerate() {
        if a.eligible && !moved[i] && occurrences.get(a.norm.as_str()).copied().unwrap_or(0) >= 2 {
            pasted[i] = true;
        }
    }

    // 4. Duplicated blocks: rolling windows of `block_min_lines` significant lines over the
    //    post-change corpus; a window seen at two non-overlapping positions marks every line
    //    it covers as duplicated.
    let k = cfg.block_min_lines.max(2);
    let min_tokens = match cfg.clone_type {
        CloneType::Exact => 0,
        CloneType::Renamed => cfg.block_min_tokens,
    };
    let mut windows: HashMap<u64, Vec<(usize, usize)>> = HashMap::new(); // hash -> (file, start pos in sig list)
    for (fi, file) in shapes.iter().enumerate() {
        if file.len() < k {
            continue;
        }
        for start in 0..=(file.len() - k) {
            let window = &file[start..start + k];
            let tokens: usize = window
                .iter()
                .map(|(_, t)| *t)
                .fold(0usize, |a, t| a.saturating_add(t));
            if tokens < min_tokens {
                continue;
            }
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for (shape, _) in window {
                shape.hash(&mut h);
                0u8.hash(&mut h);
            }
            windows.entry(h.finish()).or_default().push((fi, start));
        }
    }
    // Per file: which significant-line positions are covered by a duplicated window, and for
    // each covered window start, where its twin is (first other occurrence).
    let mut covered: Vec<Vec<bool>> = new_side.iter().map(|f| vec![false; f.len()]).collect();
    let mut twins: Vec<Vec<Option<(usize, usize)>>> =
        new_side.iter().map(|f| vec![None; f.len()]).collect();
    for positions in windows.values() {
        if positions.len() < 2 {
            continue;
        }
        for &(fi, start) in positions {
            let twin = positions
                .iter()
                .find(|&&(fj, s2)| fj != fi || s2.abs_diff(start) >= k)
                .copied();
            if let Some(t) = twin {
                for c in &mut covered[fi][start..start + k] {
                    *c = true;
                }
                if twins[fi][start].is_none() {
                    twins[fi][start] = Some(t);
                }
            }
        }
    }
    // Map new-side line index -> position in the significant list, per file.
    let sig_pos: Vec<HashMap<usize, usize>> = new_side
        .iter()
        .map(|f| {
            f.iter()
                .enumerate()
                .map(|(p, (idx, _))| (*idx, p))
                .collect()
        })
        .collect();
    let mut in_block = vec![false; added.len()];
    for (i, a) in added.iter().enumerate() {
        if !a.eligible {
            continue;
        }
        if let Some(&p) = sig_pos[a.file].get(&a.idx) {
            if covered[a.file][p] {
                in_block[i] = true;
            }
        }
    }

    // 5. Tally.
    for (i, a) in added.iter().enumerate() {
        let lang_key = a.lang.as_str().to_string();
        let c = out.by_language.entry(lang_key).or_default();
        c.added += 1;
        out.counts.added += 1;
        if moved[i] {
            c.moved += 1;
            out.counts.moved += 1;
        }
        if pasted[i] {
            c.copy_pasted += 1;
            out.counts.copy_pasted += 1;
        }
        if in_block[i] {
            c.dup_block += 1;
            out.counts.dup_block += 1;
        }
        if a.eligible && !moved[i] {
            out.added_fingerprints
                .push((a.path.to_string(), fingerprint(a.path, &a.norm)));
        }
        if want_explain {
            let e = &mut explanations[a.file];
            e.counts.added += 1;
            if moved[i] {
                e.counts.moved += 1;
                e.moved_lines.push(a.idx + 1);
            }
            if pasted[i] {
                e.counts.copy_pasted += 1;
                e.copy_pasted_lines.push(a.idx + 1);
            }
            if in_block[i] {
                e.counts.dup_block += 1;
                e.dup_block_lines.push(a.idx + 1);
            }
        }
    }
    // Duplicated blocks with their twins, for the explanation: coalesce consecutive window
    // starts that share a twin into one block.
    if want_explain {
        for (fi, file_twins) in twins.iter().enumerate() {
            // Windows overlap by construction (every start position is a window), so a
            // twelve-line clone would otherwise be reported seven times. Coalesce runs that
            // continue the same twin, then never report a block that starts inside the
            // previous reported block of the same file.
            let mut reported_end: Option<usize> = None; // exclusive, in significant-line positions
            let mut p = 0;
            while p < file_twins.len() {
                let Some((tf, ts)) = file_twins[p] else {
                    p += 1;
                    continue;
                };
                let mut end = p;
                while end + 1 < file_twins.len()
                    && file_twins[end + 1] == Some((tf, ts + (end + 1 - p)))
                {
                    end += 1;
                }
                let block_lines = end - p + k;
                let overlaps = reported_end.is_some_and(|re| p < re);
                let touches_added = added.iter().any(|a| {
                    a.file == fi
                        && sig_pos[fi]
                            .get(&a.idx)
                            .is_some_and(|&pos| pos >= p && pos < p + block_lines)
                });
                if touches_added && !overlaps {
                    explanations[fi].dup_blocks.push(DupBlock {
                        line: new_side[fi][p].0 + 1,
                        lines: block_lines,
                        other_path: commit.files[tf].path.clone(),
                        other_line: new_side[tf][ts].0 + 1,
                    });
                    reported_end = Some(p + block_lines);
                }
                p = end + 1;
            }
        }
    }
    for d in &deleted {
        out.counts.deleted += 1;
        out.by_language
            .entry(d.lang.as_str().to_string())
            .or_default()
            .deleted += 1;
        if d.eligible {
            out.deleted_fingerprints
                .push((d.path.to_string(), fingerprint(d.path, &d.norm)));
        }
        if want_explain {
            explanations[d.file].counts.deleted += 1;
        }
    }
    (out, explanations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::line::heuristic_mask;

    fn file(path: &str, old: Option<&str>, new: Option<&str>) -> FileChange {
        let lang = Language::from_path(path).unwrap_or(Language::Other);
        FileChange {
            path: path.to_string(),
            language: lang,
            old: old.map(str::to_string),
            new: new.map(str::to_string),
            old_mask: old.map(|t| heuristic_mask(lang, t)),
            new_mask: new.map(|t| heuristic_mask(lang, t)),
        }
    }

    fn commit(files: Vec<FileChange>) -> CommitInput {
        CommitInput {
            sha: "abc".into(),
            parent: Some("def".into()),
            timestamp_unix: 0,
            author: "a".into(),
            is_merge: false,
            origin: Origin::Human,
            files,
        }
    }

    const BODY: &str = "fn compute_total(items: &[Item]) -> u64 {\n    let mut total = 0;\n    for item in items {\n        total += item.price * item.qty;\n    }\n    total\n}\n";

    #[test]
    fn a_plain_addition_is_only_added() {
        let s = classify(
            &commit(vec![file("a.rs", None, Some(BODY))]),
            &SignalConfig::default(),
        );
        assert_eq!(s.counts.added, 5); // braces and `total` alone are structural
        assert_eq!(s.counts.moved, 0);
        assert_eq!(s.counts.copy_pasted, 0);
        assert_eq!(s.counts.dup_block, 0);
        assert_eq!(s.by_language["rust"].added, 5);
    }

    #[test]
    fn moving_a_function_between_files_is_refactoring() {
        let c = commit(vec![
            file("a.rs", Some(BODY), Some("")),
            file("b.rs", None, Some(BODY)),
        ]);
        let s = classify(&c, &SignalConfig::default());
        assert_eq!(s.counts.added, 5);
        assert_eq!(s.counts.deleted, 5);
        // `let mut total = 0;` is 18 chars; `total += ...` etc. all eligible; `for item in items {` is 19.
        assert_eq!(s.counts.moved, 4); // `total` alone is below min_line_chars
        assert_eq!(s.counts.copy_pasted, 0);
        assert_eq!(s.counts.dup_block, 0);
    }

    #[test]
    fn pasting_a_function_twice_is_copy_paste_and_a_duplicated_block() {
        let twice = format!("{BODY}\n{}", BODY.replace("compute_total", "compute_again"));
        let cfg = SignalConfig {
            block_min_lines: 3,
            ..Default::default()
        };
        let s = classify(&commit(vec![file("a.rs", None, Some(&twice))]), &cfg);
        assert_eq!(s.counts.added, 10);
        // The three eligible inner lines of each copy are identical text -> 6 pasted lines.
        assert_eq!(s.counts.copy_pasted, 6);
        assert!(s.counts.dup_block >= 6, "dup_block={}", s.counts.dup_block);
        assert_eq!(s.counts.moved, 0);
    }

    #[test]
    fn pasting_from_existing_code_in_the_same_file_counts() {
        let old = BODY;
        let new = format!("{BODY}\n{}", BODY.replace("compute_total", "compute_again"));
        let s = classify(
            &commit(vec![file("a.rs", Some(old), Some(&new))]),
            &SignalConfig::default(),
        );
        assert_eq!(s.counts.added, 5);
        assert_eq!(s.counts.copy_pasted, 3);
    }

    #[test]
    fn a_pasted_and_renamed_function_is_a_duplicated_block_but_not_copy_paste() {
        // The second copy renames every identifier and changes the literals: exact matching
        // sees nothing pasted, the shape matcher sees the whole block.
        let renamed = BODY
            .replace("compute_total", "compute_again")
            .replace("total", "acc")
            .replace("items", "rows")
            .replace("item", "row")
            .replace("price", "cost")
            .replace("qty", "n")
            .replace("= 0", "= 1");
        let new = format!("{BODY}\n{renamed}");
        let cfg = SignalConfig {
            block_min_lines: 3,
            block_min_tokens: 12,
            ..Default::default()
        };
        let (s, ex) = explain(&commit(vec![file("a.rs", None, Some(&new))]), &cfg);
        assert_eq!(s.counts.copy_pasted, 0, "no line is verbatim-identical");
        assert!(s.counts.dup_block >= 6, "dup_block={}", s.counts.dup_block);
        assert_eq!(ex[0].dup_blocks.len(), 2, "{:?}", ex[0].dup_blocks);
        let first = &ex[0].dup_blocks[0];
        assert_eq!(first.other_path, "a.rs");
        assert_ne!(first.line, first.other_line);
        // Exact mode is the old behaviour.
        let exact = SignalConfig {
            clone_type: CloneType::Exact,
            block_min_lines: 3,
            ..Default::default()
        };
        let s = classify(&commit(vec![file("a.rs", None, Some(&new))]), &exact);
        assert_eq!(s.counts.dup_block, 0);
    }

    #[test]
    fn repetitive_shapes_below_the_token_floor_are_not_blocks() {
        let new = "alpha_value = 1;\nbeta_value = 2;\ngamma_value = 3;\ndelta_value = 4;\nepsilon_value = 5;\nzeta_value = 6;\n";
        let cfg = SignalConfig {
            block_min_lines: 3,
            ..Default::default()
        };
        let s = classify(&commit(vec![file("a.rs", None, Some(new))]), &cfg);
        assert_eq!(
            s.counts.dup_block, 0,
            "six four-token lines never reach block_min_tokens"
        );
    }

    #[test]
    fn imports_and_short_lines_never_match() {
        let new = "use std::fmt;\nuse std::fmt;\nfoo = 1;\nfoo = 1;\n";
        let s = classify(
            &commit(vec![file("a.rs", None, Some(new))]),
            &SignalConfig::default(),
        );
        assert_eq!(s.counts.added, 4);
        assert_eq!(s.counts.copy_pasted, 0);
    }

    #[test]
    fn explain_reports_line_numbers() {
        let new = format!("{BODY}\n{}", BODY.replace("compute_total", "compute_again"));
        let (_, ex) = explain(
            &commit(vec![file("a.rs", None, Some(&new))]),
            &SignalConfig::default(),
        );
        assert_eq!(ex.len(), 1);
        assert_eq!(ex[0].copy_pasted_lines, vec![2, 3, 4, 10, 11, 12]);
    }

    #[test]
    fn masks_shorter_than_text_are_treated_as_insignificant() {
        let mut f = file("a.rs", None, Some(BODY));
        f.new_mask = Some(vec![true]);
        let s = classify(&commit(vec![f]), &SignalConfig::default());
        assert_eq!(s.counts.added, 1);
    }
}
