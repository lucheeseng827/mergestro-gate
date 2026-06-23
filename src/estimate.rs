// SPDX-License-Identifier: Apache-2.0
//! `slop-gate estimate` — project the mutant workload for a diff *without*
//! paying the build/test cycle.
//!
//! It runs the same two cheap steps the gate does up front — enumerate
//! candidates (`cargo mutants --list`, no build) and apply the per-function cap
//! — then reports how many mutants *would* be tested, broken down per function,
//! plus a rough latency band. Authors use it to predict gate cost before
//! pushing; it's also the cross-platform path, since `--list` works on Windows
//! even where a full run can't.

use std::collections::BTreeMap;

use crate::report::Mutant;

/// Rough per-mutant wall-clock band (seconds): warm `target/` → cold baseline
/// build. Sourced from `BENCHMARK.md`; a projection aid, not a guarantee.
pub const PER_MUTANT_WARM_S: f64 = 0.03;
pub const PER_MUTANT_COLD_S: f64 = 0.5;

/// Projected workload for one function (or file, when the function is unknown).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionEstimate {
    /// `file::function` grouping key the cap is applied over.
    pub group: String,
    pub file: String,
    /// First candidate line in the group — a jump-to anchor.
    pub line: u32,
    /// Mutants the enumerator found on the changed lines.
    pub candidates: usize,
    /// Mutants that would actually be tested = `min(cap, candidates)`.
    pub tested: usize,
    /// Mutants excluded by the per-function cap.
    pub capped_out: usize,
}

/// The whole-diff projection.
#[derive(Debug, Clone)]
pub struct Estimate {
    pub functions: Vec<FunctionEstimate>,
    pub total_candidates: usize,
    pub total_tested: usize,
    pub total_capped: usize,
    pub cap: usize,
}

impl Estimate {
    /// Group candidates by function, apply the cap arithmetic, and total it.
    /// Mirrors [`crate::mutants::apply_cap`]'s grouping so the projection equals
    /// what a real run would test.
    pub fn from_candidates(candidates: &[Mutant], cap: usize) -> Estimate {
        // Preserve first-seen line per group; count candidates per group.
        let mut order: Vec<String> = Vec::new();
        let mut by_group: BTreeMap<String, (String, u32, usize)> = BTreeMap::new();
        for m in candidates {
            let key = m.group_key();
            match by_group.get_mut(&key) {
                Some(entry) => {
                    entry.2 += 1;
                    if m.line < entry.1 {
                        entry.1 = m.line;
                    }
                }
                None => {
                    order.push(key.clone());
                    by_group.insert(key, (m.file.clone(), m.line, 1));
                }
            }
        }

        let mut functions: Vec<FunctionEstimate> = order
            .into_iter()
            .map(|group| {
                let (file, line, candidates) = by_group.remove(&group).expect("group present");
                let tested = candidates.min(cap);
                FunctionEstimate {
                    group,
                    file,
                    line,
                    candidates,
                    tested,
                    capped_out: candidates - tested,
                }
            })
            .collect();

        // Heaviest first — that's what an author wants to see.
        functions.sort_by(|a, b| {
            b.candidates
                .cmp(&a.candidates)
                .then_with(|| a.group.cmp(&b.group))
        });

        let total_candidates = functions.iter().map(|f| f.candidates).sum();
        let total_tested = functions.iter().map(|f| f.tested).sum();
        let total_capped = functions.iter().map(|f| f.capped_out).sum();
        Estimate {
            functions,
            total_candidates,
            total_tested,
            total_capped,
            cap,
        }
    }

    /// Low/high wall-clock projection (seconds) for the tested mutants.
    pub fn latency_band_secs(&self) -> (f64, f64) {
        let n = self.total_tested as f64;
        (n * PER_MUTANT_WARM_S, n * PER_MUTANT_COLD_S)
    }

    /// Human-readable projection.
    pub fn render_text(&self) -> String {
        let mut s = String::new();
        s.push_str("── Slop Filter · mutant estimate (dry run, no build) ──\n");
        if self.functions.is_empty() {
            s.push_str("no mutable Rust constructs on the changed lines → 0 mutants.\n");
            return s;
        }
        s.push_str(&format!(
            "{:<40} {:>6} {:>10} {:>4} {:>8}\n",
            "function", "line", "candidates", "test", "capped"
        ));
        for f in &self.functions {
            let label = display_group(&f.group);
            s.push_str(&format!(
                "{:<40} {:>6} {:>10} {:>4} {:>8}\n",
                truncate(&label, 40),
                f.line,
                f.candidates,
                f.tested,
                if f.capped_out > 0 {
                    f.capped_out.to_string()
                } else {
                    "-".into()
                },
            ));
        }
        let (lo, hi) = self.latency_band_secs();
        s.push_str(&format!(
            "\ntotal: {} candidate(s) → {} tested ({} capped at {}/fn across {} group(s))\n",
            self.total_candidates,
            self.total_tested,
            self.total_capped,
            self.cap,
            self.functions.len(),
        ));
        s.push_str(&format!(
            "est. mutation time: ~{lo:.1}s (warm target/) … ~{hi:.1}s (cold build)  [rough band; see BENCHMARK.md]\n",
        ));
        s
    }

    /// Machine-readable projection (stable keys, for CI/tooling).
    pub fn render_json(&self) -> String {
        let (lo, hi) = self.latency_band_secs();
        let funcs: Vec<String> = self
            .functions
            .iter()
            .map(|f| {
                format!(
                    r#"{{"group":{},"file":{},"line":{},"candidates":{},"tested":{},"capped_out":{}}}"#,
                    json_str(&f.group),
                    json_str(&f.file),
                    f.line,
                    f.candidates,
                    f.tested,
                    f.capped_out,
                )
            })
            .collect();
        format!(
            r#"{{"total_candidates":{},"total_tested":{},"total_capped":{},"cap_per_function":{},"est_seconds_warm":{:.2},"est_seconds_cold":{:.2},"functions":[{}]}}"#,
            self.total_candidates,
            self.total_tested,
            self.total_capped,
            self.cap,
            lo,
            hi,
            funcs.join(","),
        )
    }
}

/// `file::function` → a compact `function (file)` label; `<file>` groups show
/// just the file.
fn display_group(group: &str) -> String {
    match group.split_once("::") {
        Some((file, "<file>")) => file.to_string(),
        Some((file, func)) => format!("{func} ({file})"),
        None => group.to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// Minimal JSON string escaping for the fields we emit (paths, identifiers).
/// Escapes the JSON-mandated control characters so `--json` stays valid even if
/// a path/identifier somehow contains a control byte.
fn json_str(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mutant(file: &str, line: u32, func: Option<&str>) -> Mutant {
        Mutant {
            file: file.into(),
            line,
            column: 1,
            function: func.map(str::to_string),
            description: "replace x".into(),
            name: format!("{file}:{line}:1: replace x"),
        }
    }

    #[test]
    fn caps_per_function_and_totals() {
        // foo: 7 candidates (capped to 5), bar: 2 (untouched).
        let mut cands = Vec::new();
        for i in 0..7 {
            cands.push(mutant("src/a.rs", 10 + i, Some("foo")));
        }
        cands.push(mutant("src/a.rs", 40, Some("bar")));
        cands.push(mutant("src/a.rs", 41, Some("bar")));

        let est = Estimate::from_candidates(&cands, 5);
        assert_eq!(est.total_candidates, 9);
        assert_eq!(est.total_tested, 7); // 5 + 2
        assert_eq!(est.total_capped, 2);
        // Heaviest group first.
        assert_eq!(est.functions[0].group, "src/a.rs::foo");
        assert_eq!(est.functions[0].tested, 5);
        assert_eq!(est.functions[0].capped_out, 2);
        assert_eq!(est.functions[0].line, 10); // earliest line retained
        assert_eq!(est.functions[1].tested, 2);
        assert_eq!(est.functions[1].capped_out, 0);
    }

    #[test]
    fn empty_diff_estimates_zero() {
        let est = Estimate::from_candidates(&[], 5);
        assert_eq!(est.total_tested, 0);
        assert!(est.functions.is_empty());
        assert!(est.render_text().contains("0 mutants"));
    }

    #[test]
    fn latency_band_scales_with_tested() {
        let cands = vec![mutant("a.rs", 1, Some("f")), mutant("a.rs", 2, Some("f"))];
        let est = Estimate::from_candidates(&cands, 5);
        let (lo, hi) = est.latency_band_secs();
        assert!((lo - 2.0 * PER_MUTANT_WARM_S).abs() < 1e-9);
        assert!((hi - 2.0 * PER_MUTANT_COLD_S).abs() < 1e-9);
    }

    #[test]
    fn json_has_stable_keys() {
        let cands = vec![mutant("src/a.rs", 3, Some("f"))];
        let json = Estimate::from_candidates(&cands, 5).render_json();
        assert!(json.contains(r#""total_tested":1"#));
        assert!(json.contains(r#""cap_per_function":5"#));
        assert!(json.contains(r#""group":"src/a.rs::f""#));
    }

    #[test]
    fn json_str_escapes_control_chars() {
        assert_eq!(json_str("a\tb"), "\"a\\tb\"");
        assert_eq!(json_str("a\rb"), "\"a\\rb\"");
        // A bare control byte must become a \u escape, not leak raw.
        let escaped = json_str("a\u{1}b");
        assert!(escaped.contains("\\u0001"));
        assert!(!escaped.chars().any(|c| (c as u32) < 0x20));
    }
}
