// SPDX-License-Identifier: Apache-2.0
//! The verdict engine: turn a [`GateReport`] plus config into a pass/block
//! decision. This is what makes Phase 2 *a gate* rather than advisory.
//!
//! Hard gates block; soft signals only score. By default survivors block
//! (above `max_survivors`) and an untrustworthy suite blocks; the
//! zero-assertion check, the severity tier, and the debt-delta are advisory
//! unless explicitly turned into gates (Phase 4).

use crate::config::Config;
use crate::pattern::PatternReport;
use crate::report::{GateReport, Verdict};
use crate::severity::SeverityCounts;

/// Decide the verdict for a completed (but not-yet-finalised) report.
pub fn decide(report: &GateReport, cfg: &Config) -> Verdict {
    let mut reasons = Vec::new();

    if cfg.block_on_survivors {
        // A suite that isn't green & stable means we can't certify the change —
        // surviving mutants would be indistinguishable from suite flakiness.
        // Only relevant once there's Rust to mutate.
        if !report.changed_rust_files.is_empty() && !report.preflight.is_green() {
            reasons.push(
                "test suite was not green & stable, so mutation results can't be trusted"
                    .to_string(),
            );
        } else if report.survivors.len() > cfg.max_survivors {
            reasons.push(format!(
                "{} surviving mutation(s) exceed the allowed {}",
                report.survivors.len(),
                cfg.max_survivors
            ));
        }
    }

    if cfg.block_on_zero_assertion_tests && !report.zero_assertion_tests.is_empty() {
        reasons.push(format!(
            "{} test(s) on the changed surface assert nothing",
            report.zero_assertion_tests.len()
        ));
    }

    // Phase 4: severity gate — block when a survivor reaches a chosen tier even
    // if the count is within budget (an unnoticed auth-bypass shouldn't slip
    // through just because it's the only survivor).
    if let Some(threshold) = cfg.block_on_severity {
        let counts = SeverityCounts::of(&report.survivors);
        if let Some(worst) = counts.max_tier() {
            if worst >= threshold {
                reasons.push(format!(
                    "a {} severity survivor was found (block threshold: {})",
                    worst.label(),
                    threshold.label()
                ));
            }
        }
    }

    // Phase 4: debt-delta gate — block when the change adds more structural debt
    // than the per-PR budget allows.
    if cfg.block_on_debt {
        if let Some(debt) = &report.debt {
            if debt.over_budget(cfg.debt_budget) {
                reasons.push(format!(
                    "debt-delta {} exceeds the budget of {}",
                    debt.score(),
                    cfg.debt_budget
                ));
            }
        }
    }

    // Track B: pattern-lane gate — opt-in. Each configured target (a lane name
    // or a specific rule id) blocks when matched. Advisory unless requested.
    for reason in pattern_block_reasons(report, &cfg.block_on_pattern) {
        if !reasons.contains(&reason) {
            reasons.push(reason);
        }
    }

    if reasons.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Block { reasons }
    }
}

/// Block reasons from the pattern lanes for the configured targets. A target is
/// a lane name (`slop`/`security`/`convention`/`all`) or a rule id; lane names
/// gate on any finding in that lane, rule ids gate on a matching finding in any
/// lane.
fn pattern_block_reasons(report: &GateReport, targets: &[String]) -> Vec<String> {
    let lanes: [(&str, &Option<PatternReport>); 4] = [
        ("slop", &report.slop),
        ("security", &report.security),
        ("convention", &report.convention),
        ("docs", &report.docs),
    ];
    let mut reasons = Vec::new();
    for target in targets {
        let t = target.trim();
        if t == "all" {
            for (name, rep) in &lanes {
                if let Some(r) = rep {
                    if !r.findings.is_empty() {
                        reasons.push(lane_reason(name, r.findings.len()));
                    }
                }
            }
        } else if let Some((name, rep)) = lanes.iter().find(|(name, _)| *name == t) {
            if let Some(r) = rep {
                if !r.findings.is_empty() {
                    reasons.push(lane_reason(name, r.findings.len()));
                }
            }
        } else {
            // Treat as a rule id: count matches across every lane.
            let hits = lanes
                .iter()
                .filter_map(|(_, rep)| rep.as_ref())
                .flat_map(|r| r.findings.iter())
                .filter(|f| f.rule == t)
                .count();
            if hits > 0 {
                reasons.push(format!(
                    "pattern rule `{t}` matched {hits} time(s) on the changed surface (block-on-pattern)"
                ));
            }
        }
    }
    reasons
}

fn lane_reason(lane: &str, n: usize) -> String {
    format!("pattern lane `{lane}` flagged {n} finding(s) (block-on-pattern)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Mutant, PreflightOutcome};

    fn report_with_survivors(n: usize) -> GateReport {
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.preflight = PreflightOutcome::Passed { runs: 2 };
        r.survivors = (0..n)
            .map(|i| Mutant::parse_name(&format!("src/x.rs:{}:1: replace x", i + 1)).unwrap())
            .collect();
        r
    }

    #[test]
    fn clean_run_passes() {
        let r = report_with_survivors(0);
        assert_eq!(decide(&r, &Config::default()), Verdict::Pass);
    }

    #[test]
    fn any_survivor_blocks_by_default() {
        let r = report_with_survivors(1);
        assert!(decide(&r, &Config::default()).is_block());
    }

    #[test]
    fn survivors_within_threshold_pass() {
        let r = report_with_survivors(2);
        let cfg = Config {
            max_survivors: 2,
            ..Config::default()
        };
        assert_eq!(decide(&r, &cfg), Verdict::Pass);
    }

    #[test]
    fn advisory_mode_never_blocks_on_survivors() {
        let r = report_with_survivors(5);
        let cfg = Config {
            block_on_survivors: false,
            ..Config::default()
        };
        assert_eq!(decide(&r, &cfg), Verdict::Pass);
    }

    #[test]
    fn red_suite_blocks_when_gating() {
        let mut r = report_with_survivors(0);
        r.preflight = PreflightOutcome::Failed {
            run: 1,
            detail: "boom".into(),
        };
        let v = decide(&r, &Config::default());
        assert!(v.is_block());
        match v {
            Verdict::Block { reasons } => assert!(reasons[0].contains("not green")),
            _ => unreachable!(),
        }
    }

    #[test]
    fn severity_gate_blocks_a_critical_survivor_within_budget() {
        use crate::report::Mutant;
        use crate::severity::Severity;

        // One survivor, but it's a control-flow mutation in an auth path → critical.
        let mut r = report_with_survivors(0);
        r.survivors = vec![Mutant {
            file: "src/auth/check.rs".into(),
            line: 1,
            column: 1,
            function: Some("can_access".into()),
            description: "replace > with >=".into(),
            name: "src/auth/check.rs:1:1: replace > with >=".into(),
        }];
        // Allow one survivor by count, but gate on severity.
        let cfg = Config {
            max_survivors: 1,
            block_on_severity: Some(Severity::High),
            ..Config::default()
        };
        let v = decide(&r, &cfg);
        assert!(v.is_block());
        match v {
            Verdict::Block { reasons } => {
                assert!(reasons.iter().any(|r| r.contains("critical severity")))
            }
            _ => unreachable!(),
        }

        // Without the severity gate, the same report passes (within count budget).
        let advisory = Config {
            max_survivors: 1,
            ..Config::default()
        };
        assert_eq!(decide(&r, &advisory), Verdict::Pass);
    }

    #[test]
    fn debt_gate_is_advisory_by_default_but_blocks_when_enabled() {
        use crate::debt::DebtDelta;

        let mut r = report_with_survivors(0);
        r.debt = Some(DebtDelta {
            complexity: 30,
            ..DebtDelta::default()
        });
        // Advisory: debt over budget doesn't block on its own.
        let advisory = Config {
            debt_budget: 25,
            ..Config::default()
        };
        assert_eq!(decide(&r, &advisory), Verdict::Pass);
        // Gated: blocks once enabled.
        let gated = Config {
            debt_budget: 25,
            block_on_debt: true,
            ..Config::default()
        };
        assert!(decide(&r, &gated).is_block());
    }

    fn pattern(rule: &str) -> crate::pattern::PatternReport {
        crate::pattern::PatternReport::from_findings(vec![crate::pattern::PatternFinding {
            rule: rule.into(),
            file: "src/x.rs".into(),
            line: 1,
            message: "m".into(),
            weight: 40,
        }])
    }

    #[test]
    fn pattern_lanes_advisory_by_default() {
        let mut r = report_with_survivors(0);
        r.security = Some(pattern("hardcoded-secret"));
        r.slop = Some(pattern("redundant-wrapper"));
        // No block_on_pattern → pattern findings never gate.
        assert_eq!(decide(&r, &Config::default()), Verdict::Pass);
    }

    #[test]
    fn docs_lane_gates_when_targeted() {
        // The docs lane participates in block_on_pattern exactly like the
        // other pattern lanes: advisory by default, gating when named.
        let mut report = GateReport::new("a", "b");
        report.docs = Some(PatternReport::from_findings(vec![
            crate::pattern::PatternFinding {
                rule: "docs-stale-config".into(),
                file: "mods/x/src/main.rs".into(),
                line: 0,
                message: "config surface without doc touch".into(),
                weight: 15,
            },
        ]));

        // Not targeted → advisory.
        let cfg = Config::default();
        assert!(!decide(&report, &cfg).is_block());

        // Lane name gates.
        let cfg = Config {
            block_on_pattern: vec!["docs".into()],
            ..Config::default()
        };
        assert!(decide(&report, &cfg).is_block());

        // Rule id gates.
        let cfg = Config {
            block_on_pattern: vec!["docs-stale-config".into()],
            ..Config::default()
        };
        assert!(decide(&report, &cfg).is_block());
    }

    #[test]
    fn block_on_pattern_lane_name_gates() {
        let mut r = report_with_survivors(0);
        r.security = Some(pattern("hardcoded-secret"));
        let cfg = Config {
            block_on_pattern: vec!["security".into()],
            ..Config::default()
        };
        let v = decide(&r, &cfg);
        assert!(v.is_block());
        match v {
            Verdict::Block { reasons } => {
                assert!(reasons.iter().any(|x| x.contains("lane `security`")))
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn block_on_pattern_rule_id_gates_across_lanes() {
        let mut r = report_with_survivors(0);
        r.convention = Some(pattern("unknown-crate-import"));
        let cfg = Config {
            block_on_pattern: vec!["unknown-crate-import".into()],
            ..Config::default()
        };
        let v = decide(&r, &cfg);
        assert!(v.is_block());
        match v {
            Verdict::Block { reasons } => {
                assert!(reasons
                    .iter()
                    .any(|x| x.contains("rule `unknown-crate-import`")))
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn block_on_pattern_does_not_gate_a_different_target() {
        let mut r = report_with_survivors(0);
        r.slop = Some(pattern("redundant-wrapper"));
        // Gating on security only — a slop finding must not block.
        let cfg = Config {
            block_on_pattern: vec!["security".into(), "hardcoded-secret".into()],
            ..Config::default()
        };
        assert_eq!(decide(&r, &cfg), Verdict::Pass);
    }

    #[test]
    fn block_on_pattern_all_gates_any_lane() {
        let mut r = report_with_survivors(0);
        r.convention = Some(pattern("unknown-crate-import"));
        let cfg = Config {
            block_on_pattern: vec!["all".into()],
            ..Config::default()
        };
        assert!(decide(&r, &cfg).is_block());
    }

    #[test]
    fn zero_assertion_advisory_by_default_but_gates_when_enabled() {
        let mut r = report_with_survivors(0);
        r.zero_assertion_tests = vec![crate::report::ZeroAssertionFinding {
            file: "src/x.rs".into(),
            line: 3,
            function: "theater".into(),
        }];
        // Advisory by default.
        assert_eq!(decide(&r, &Config::default()), Verdict::Pass);
        // Gates when turned on.
        let cfg = Config {
            block_on_zero_assertion_tests: true,
            ..Config::default()
        };
        assert!(decide(&r, &cfg).is_block());
    }
}
