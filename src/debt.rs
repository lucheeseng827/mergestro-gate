// SPDX-License-Identifier: Apache-2.0
//! Phase 4 — debt-delta budget.
//!
//! Mutation testing is a *point-in-time* behavioural check. The debt-delta turns
//! the gate into something closer to an **SLO**: it measures how much structural
//! debt a change *adds*, frames it against a per-PR budget, and reports the
//! burn-rate. Tracked over time these deltas are a trajectory — the "error
//! budget" wedge the build plan calls out as what makes the gate more than a
//! single check.
//!
//! It is computed straight from the unified diff the gate already produces for
//! `cargo-mutants --in-diff`, so it costs nothing extra and is a *delta* by
//! construction: every signal is `added − removed`. Three cheap proxies:
//!
//! - **complexity** — net decision points added (`if`, `match`, `for`, `while`,
//!   `&&`, `||`, `?`): branching is what makes code hard to test and reason about.
//! - **duplication** — net repeated added lines (copy-paste within the change).
//! - **coupling** — net `use` imports added (more edges into other modules).
//!
//! The score is the sum; a change that deletes as much debt as it adds nets out
//! near zero. By default this is advisory (it scores, it doesn't block); a team
//! can opt into blocking once the delta exceeds budget.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The net structural-debt change introduced by a diff. Each field is
/// `added − removed`, so negative values mean the change *paid down* debt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebtDelta {
    /// Net decision points (branching keywords / short-circuit operators).
    pub complexity: i64,
    /// Net duplicated added lines (repeats within the changed surface).
    pub duplication: i64,
    /// Net `use` imports (fan-out into other modules).
    pub coupling: i64,
    /// Total `+` lines in the diff (excluding file headers).
    pub added_lines: usize,
    /// Total `-` lines in the diff (excluding file headers).
    pub removed_lines: usize,
}

impl DebtDelta {
    /// The headline figure: net debt added across all three proxies.
    pub fn score(&self) -> i64 {
        self.complexity + self.duplication + self.coupling
    }

    /// Burn-rate against a budget: `score / budget`. `1.0` means the change
    /// spends exactly its budget; above `1.0` is over. `None` if budget <= 0.
    pub fn burn_rate(&self, budget: i64) -> Option<f64> {
        if budget <= 0 {
            None
        } else {
            Some(self.score() as f64 / budget as f64)
        }
    }

    /// Whether the net debt added exceeds `budget` (only meaningful for a
    /// positive budget; a non-positive budget disables the check).
    pub fn over_budget(&self, budget: i64) -> bool {
        budget > 0 && self.score() > budget
    }
}

/// Compute the debt-delta from a unified diff (the same text fed to
/// `cargo-mutants --in-diff`). Hunk/file-header lines are ignored; only true
/// added/removed content lines contribute.
pub fn from_unified_diff(diff: &str) -> DebtDelta {
    let mut added_dup = DupCounter::default();
    let mut removed_dup = DupCounter::default();
    let mut d = DebtDelta::default();

    for line in diff.lines() {
        match classify_line(line) {
            Some((Side::Added, content)) => {
                d.added_lines += 1;
                d.complexity += decision_points(content);
                d.coupling += is_import(content) as i64;
                added_dup.record(content);
            }
            Some((Side::Removed, content)) => {
                d.removed_lines += 1;
                d.complexity -= decision_points(content);
                d.coupling -= is_import(content) as i64;
                removed_dup.record(content);
            }
            None => {}
        }
    }

    d.duplication = added_dup.duplicates() - removed_dup.duplicates();
    d
}

enum Side {
    Added,
    Removed,
}

/// Classify a raw diff line into added/removed content, filtering out the
/// `+++`/`---` file headers and `@@` hunk headers that share the prefix.
fn classify_line(line: &str) -> Option<(Side, &str)> {
    if line.starts_with("+++") || line.starts_with("---") {
        return None;
    }
    if let Some(rest) = line.strip_prefix('+') {
        Some((Side::Added, rest))
    } else if let Some(rest) = line.strip_prefix('-') {
        Some((Side::Removed, rest))
    } else {
        None
    }
}

/// Count decision points in one line of source. Keyword markers are matched
/// with surrounding word boundaries so `verify` doesn't match `if`; operators
/// are counted by occurrence.
fn decision_points(line: &str) -> i64 {
    let mut n = 0i64;
    for kw in ["if", "match", "for", "while"] {
        n += count_word(line, kw) as i64;
    }
    n += line.matches("&&").count() as i64;
    n += line.matches("||").count() as i64;
    n += line.matches('?').count() as i64;
    n
}

/// Count whole-word occurrences of `word` in `line` (boundaries are non
/// alphanumeric / non-`_`), so keywords inside identifiers aren't counted.
fn count_word(line: &str, word: &str) -> usize {
    let bytes = line.as_bytes();
    let mut count = 0;
    let mut start = 0;
    while let Some(pos) = line[start..].find(word) {
        let i = start + pos;
        let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
        let after = i + word.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            count += 1;
        }
        start = i + word.len();
    }
    count
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whether a source line is a `use` import (after leading whitespace).
fn is_import(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("use ") || t.starts_with("pub use ")
}

/// Tallies repeated, non-trivial lines so duplication can be measured as
/// "extra copies": a line seen `k` times contributes `k - 1`.
#[derive(Default)]
struct DupCounter {
    seen: HashMap<String, usize>,
}

impl DupCounter {
    fn record(&mut self, line: &str) {
        let t = line.trim();
        // Ignore trivial lines (closing braces, short fragments) that repeat
        // legitimately and would swamp the signal.
        if t.len() < 12 {
            return;
        }
        *self.seen.entry(t.to_string()).or_insert(0) += 1;
    }

    fn duplicates(&self) -> i64 {
        self.seen
            .values()
            .map(|&k| (k.saturating_sub(1)) as i64)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_diff_is_zero() {
        let d = from_unified_diff("");
        assert_eq!(d.score(), 0);
        assert_eq!(d.added_lines, 0);
        assert!(!d.over_budget(10));
    }

    #[test]
    fn counts_net_complexity_added() {
        // Two added branches, one removed branch → net +1 decision point.
        let diff = "\
diff --git a/src/x.rs b/src/x.rs
--- a/src/x.rs
+++ b/src/x.rs
@@ -1,3 +1,4 @@
-    if a { 1 } else { 0 }
+    if a && b {
+        while c { work() }
+    }
";
        let d = from_unified_diff(diff);
        // added: `if`+`&&` (2) and `while` (1) = 3; removed: `if` (1). net = 2.
        assert_eq!(d.complexity, 2);
        assert_eq!(d.added_lines, 3);
        assert_eq!(d.removed_lines, 1);
    }

    #[test]
    fn keywords_inside_identifiers_are_not_counted() {
        let diff = "+    let verifier = formatter(modifier);\n";
        // `if`/`for`/`while`/`match` must not match inside these identifiers.
        assert_eq!(from_unified_diff(diff).complexity, 0);
    }

    #[test]
    fn coupling_counts_net_imports() {
        let diff = "\
+use std::collections::HashMap;
+    pub use crate::thing::Other;
-use std::io::Write;
";
        assert_eq!(from_unified_diff(diff).coupling, 1); // +2 − 1
    }

    #[test]
    fn duplication_counts_repeated_added_lines() {
        let diff = "\
+    let total = compute_the_value(input);
+    let total = compute_the_value(input);
+    let total = compute_the_value(input);
";
        // Same non-trivial line added 3× → 2 extra copies.
        assert_eq!(from_unified_diff(diff).duplication, 2);
    }

    #[test]
    fn header_lines_are_ignored() {
        let diff = "--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n";
        let d = from_unified_diff(diff);
        assert_eq!(d.added_lines, 0);
        assert_eq!(d.removed_lines, 0);
    }

    #[test]
    fn budget_and_burn_rate() {
        let d = DebtDelta {
            complexity: 20,
            duplication: 5,
            coupling: 5,
            ..DebtDelta::default()
        };
        assert_eq!(d.score(), 30);
        assert!(d.over_budget(25));
        assert!(!d.over_budget(30)); // strictly greater
        assert_eq!(d.burn_rate(25), Some(1.2));
        assert_eq!(d.burn_rate(0), None);
    }
}
