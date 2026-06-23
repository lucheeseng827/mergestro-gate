// SPDX-License-Identifier: Apache-2.0
//! Phase 4 — severity ranking of survivors.
//!
//! Not every surviving mutation is equally dangerous. A flipped `>=` inside a
//! permission check that the tests passed over is a latent auth bypass; the same
//! mutation inside a log-line formatter is cosmetic. Phase 4 ranks survivors so
//! the most dangerous ones surface first in the report and the PR comment, and
//! so a team rolling the gate out can choose to **block only at or above a
//! severity tier** while still merely reporting the long tail.
//!
//! The classifier is a transparent heuristic over two cheap signals:
//!
//! 1. **Where** the mutation lives — the file path and (when known) the
//!    enclosing function. Security-sensitive locations (auth, permissions,
//!    crypto, validation) raise severity; cosmetic ones (logging, formatting)
//!    lower it.
//! 2. **What** the mutation does — control-flow mutations (booleans, comparison
//!    and logical operators, negation) are riskier than arithmetic or a whole
//!    function being stubbed out.
//!
//! It is deliberately explainable rather than clever: the goal is a defensible
//! ordering and an opt-in gate, not a precise risk score.

use serde::{Deserialize, Serialize};

use crate::report::Mutant;

/// Risk tier for a surviving mutation. Ordering is `Low < Medium < High <
/// Critical` (declaration order), so survivors sort by descending severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// Lowercase label used in reports, config, and telemetry.
    pub fn label(self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }

    /// Parse a tier from a case-insensitive label (for CLI / config input).
    pub fn parse(s: &str) -> Option<Severity> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Severity::Low),
            "medium" | "med" => Some(Severity::Medium),
            "high" => Some(Severity::High),
            "critical" | "crit" => Some(Severity::Critical),
            _ => None,
        }
    }

    /// One tier up, saturating at `Critical`.
    fn up(self) -> Severity {
        match self {
            Severity::Low => Severity::Medium,
            Severity::Medium => Severity::High,
            Severity::High | Severity::Critical => Severity::Critical,
        }
    }

    /// One tier down, saturating at `Low`.
    fn down(self) -> Severity {
        match self {
            Severity::Critical => Severity::High,
            Severity::High => Severity::Medium,
            Severity::Medium | Severity::Low => Severity::Low,
        }
    }
}

/// Substrings that mark a location as security-sensitive (matched against the
/// lowercased file path and enclosing function). A surviving mutation here is a
/// candidate latent vulnerability, so it is ranked up.
const SECURITY_HINTS: &[&str] = &[
    "auth",
    "authz",
    "authn",
    "permission",
    "perm",
    "acl",
    "access",
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "crypto",
    "cipher",
    "signature",
    "verify",
    "validate",
    "sanitize",
    "escape",
    "guard",
    "session",
    "login",
    "privilege",
    "role",
    "admin",
    "security",
    "policy",
];

/// Substrings that mark a location as cosmetic — mutations here rarely change
/// behaviour that matters, so they are ranked down.
const COSMETIC_HINTS: &[&str] = &[
    "log",
    "logger",
    "logging",
    "trace",
    "debug",
    "print",
    "format",
    "display",
    "fmt",
    "metric",
    "telemetry",
    "render",
    "comment",
];

/// Classify a single survivor into a risk tier.
pub fn classify(m: &Mutant) -> Severity {
    let location = location_text(m);
    let desc = m.description.to_ascii_lowercase();
    let base = base_from_mutation(&desc);

    // A security-sensitive location dominates: an unnoticed mutation there is
    // the auth-bypass case the ranking exists to surface.
    if mentions_any(&location, SECURITY_HINTS) {
        return base.up();
    }
    // Cosmetic locations damp the noise from log/format churn.
    if mentions_any(&location, COSMETIC_HINTS) {
        return base.down();
    }
    base
}

/// Survivors paired with their severity, sorted most-dangerous first (ties keep
/// the original — already file/line-sorted — order, so output is stable).
pub fn rank(survivors: &[Mutant]) -> Vec<(Severity, &Mutant)> {
    let mut ranked: Vec<(Severity, &Mutant)> = survivors.iter().map(|m| (classify(m), m)).collect();
    // `slice::sort_by` is a *stable* sort (unlike `sort_unstable_by`), so equal
    // severities keep their incoming file/line order — the stability the doc
    // comment promises.
    ranked.sort_by(|a, b| b.0.cmp(&a.0));
    ranked
}

/// A histogram of survivor severities — carried in telemetry so the trend
/// dashboard can show the risk mix over time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeverityCounts {
    pub critical: usize,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
}

impl SeverityCounts {
    /// Count `survivors` by tier.
    pub fn of(survivors: &[Mutant]) -> SeverityCounts {
        let mut c = SeverityCounts::default();
        for m in survivors {
            c.add(classify(m));
        }
        c
    }

    /// The highest tier with a non-zero count, if any.
    pub fn max_tier(&self) -> Option<Severity> {
        if self.critical > 0 {
            Some(Severity::Critical)
        } else if self.high > 0 {
            Some(Severity::High)
        } else if self.medium > 0 {
            Some(Severity::Medium)
        } else if self.low > 0 {
            Some(Severity::Low)
        } else {
            None
        }
    }

    /// Add `other` into this histogram (used when aggregating across runs).
    pub fn merge(&mut self, other: &SeverityCounts) {
        self.critical += other.critical;
        self.high += other.high;
        self.medium += other.medium;
        self.low += other.low;
    }

    fn add(&mut self, s: Severity) {
        match s {
            Severity::Critical => self.critical += 1,
            Severity::High => self.high += 1,
            Severity::Medium => self.medium += 1,
            Severity::Low => self.low += 1,
        }
    }
}

/// Base tier from the mutation kind alone, before location adjustments.
/// Control-flow mutations (the ones that silently change a decision) start
/// High; everything else starts Medium.
fn base_from_mutation(desc: &str) -> Severity {
    if is_control_flow(desc) {
        Severity::High
    } else {
        Severity::Medium
    }
}

/// Whether the mutation description denotes a control-flow / decision change:
/// booleans substituted, comparison or logical operators swapped, or a negation
/// deleted. These are the mutations that flip a branch without a compile error.
fn is_control_flow(desc: &str) -> bool {
    const MARKERS: &[&str] = &[
        // Rust / cargo-mutants descriptions.
        "with true",
        "with false",
        "&&",
        "||",
        "==",
        "!=",
        "<=",
        ">=",
        "delete !",
        "replace !",
        " < ",
        " > ",
        // Python / cosmic-ray operator names (lowercased): comparison, boolean
        // and logical mutations flip a decision the same way.
        "comparison",
        "boolean",
        "logical",
        "andwith",
        "orwith",
        "truewith",
        "falsewith",
    ];
    MARKERS.iter().any(|m| desc.contains(m))
}

/// Lowercased text to scan for location hints: the file path plus the enclosing
/// function name when `cargo-mutants` provided one.
fn location_text(m: &Mutant) -> String {
    let mut s = m.file.to_ascii_lowercase();
    if let Some(f) = &m.function {
        s.push(' ');
        s.push_str(&f.to_ascii_lowercase());
    }
    s
}

fn mentions_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mutant(file: &str, desc: &str, function: Option<&str>) -> Mutant {
        Mutant {
            file: file.into(),
            line: 1,
            column: 1,
            function: function.map(Into::into),
            description: desc.into(),
            name: format!("{file}:1:1: {desc}"),
        }
    }

    #[test]
    fn ordering_is_low_to_critical() {
        assert!(Severity::Low < Severity::Medium);
        assert!(Severity::Medium < Severity::High);
        assert!(Severity::High < Severity::Critical);
    }

    #[test]
    fn parse_round_trips_labels() {
        for s in [
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ] {
            assert_eq!(Severity::parse(s.label()), Some(s));
        }
        assert_eq!(Severity::parse("CRITICAL"), Some(Severity::Critical));
        assert_eq!(Severity::parse("nonsense"), None);
    }

    #[test]
    fn control_flow_in_security_location_is_critical() {
        // A comparison flipped inside a permission check: the headline case.
        let m = mutant(
            "src/auth/permissions.rs",
            "replace > with >=",
            Some("can_access"),
        );
        assert_eq!(classify(&m), Severity::Critical);
    }

    #[test]
    fn control_flow_elsewhere_is_high() {
        let m = mutant("src/math.rs", "replace && with ||", None);
        assert_eq!(classify(&m), Severity::High);
    }

    #[test]
    fn arithmetic_is_medium_by_default() {
        let m = mutant("src/calc.rs", "replace + with -", None);
        assert_eq!(classify(&m), Severity::Medium);
    }

    #[test]
    fn cosmetic_location_damps_severity() {
        // Control-flow base (High) in a logging path drops a tier.
        let m = mutant("src/logging.rs", "replace == with !=", Some("format_line"));
        assert_eq!(classify(&m), Severity::Medium);
        // Plain arithmetic in a logger is Low.
        let m2 = mutant("src/logger.rs", "replace + with *", None);
        assert_eq!(classify(&m2), Severity::Low);
    }

    #[test]
    fn rank_orders_most_dangerous_first() {
        let survivors = vec![
            mutant("src/util.rs", "replace + with -", None), // Medium
            mutant("src/auth.rs", "replace > with >=", None), // Critical
            mutant("src/math.rs", "replace && with ||", None), // High
        ];
        let ranked = rank(&survivors);
        let tiers: Vec<Severity> = ranked.iter().map(|(s, _)| *s).collect();
        assert_eq!(
            tiers,
            vec![Severity::Critical, Severity::High, Severity::Medium]
        );
    }

    #[test]
    fn counts_and_max_tier() {
        let survivors = vec![
            mutant("src/auth.rs", "replace > with >=", None), // Critical
            mutant("src/math.rs", "replace && with ||", None), // High
            mutant("src/math.rs", "replace || with &&", None), // High
            mutant("src/util.rs", "replace + with -", None),  // Medium
        ];
        let c = SeverityCounts::of(&survivors);
        assert_eq!(c.critical, 1);
        assert_eq!(c.high, 2);
        assert_eq!(c.medium, 1);
        assert_eq!(c.low, 0);
        assert_eq!(c.max_tier(), Some(Severity::Critical));
        assert_eq!(SeverityCounts::default().max_tier(), None);
    }
}
