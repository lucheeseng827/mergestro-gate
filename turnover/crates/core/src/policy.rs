//! The gate. A policy is two kinds of limit on the four ratios:
//!
//! * **absolute** — the window's ratio may not exceed (or, for refactor, fall below) a cap.
//!   Useful, but it is the part every per-commit linter already has.
//! * **drift** — the window's ratio may not move away from the repository's *own baseline*
//!   by more than a delta. This is the longitudinal gate: it fails a build because the
//!   codebase is getting worse *than it was*, not because it crossed a universal number.
//!
//! Every limit is optional; an unset one is not evaluated. Below `min_added_lines` the gate
//! reports **insufficient sample** and passes — a three-line PR must never be failed by a
//! ratio, or the number becomes noise and the team learns to ignore it.

use serde::{Deserialize, Serialize};

use crate::window::{Aggregate, Ratios};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Gate {
    /// Trailing window the gate evaluates, ending at the newest commit.
    pub window_days: u32,
    /// Significant added lines the window must contain before any limit is evaluated.
    pub min_added_lines: u64,
    /// `blocking` (non-zero exit on fail) or `advisory` (report only, always exit 0).
    pub mode: Mode,
}

impl Default for Gate {
    fn default() -> Self {
        Gate {
            window_days: 90,
            min_added_lines: 200,
            mode: Mode::Blocking,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Blocking,
    Advisory,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Blocking => "blocking",
            Mode::Advisory => "advisory",
        }
    }
}

/// Absolute caps. `refactor_min` is a floor: refactoring going to zero is the signal.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Thresholds {
    pub copy_paste_max: Option<f64>,
    pub dup_block_max: Option<f64>,
    pub churn_max: Option<f64>,
    pub refactor_min: Option<f64>,
}

/// Allowed movement from the baseline ratio. Positive numbers; direction is per signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Drift {
    pub copy_paste_max_rise: Option<f64>,
    pub dup_block_max_rise: Option<f64>,
    pub churn_max_rise: Option<f64>,
    pub refactor_max_drop: Option<f64>,
}

impl Default for Drift {
    fn default() -> Self {
        Drift {
            copy_paste_max_rise: Some(0.05),
            dup_block_max_rise: Some(0.05),
            churn_max_rise: Some(0.05),
            refactor_max_drop: Some(0.05),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub gate: Gate,
    pub thresholds: Thresholds,
    pub drift: Drift,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
    InsufficientSample,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Absolute,
    Drift,
}

/// One evaluated limit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub signal: String,
    pub kind: CheckKind,
    pub observed: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<f64>,
    /// The bound the observed value was held to (already baseline-adjusted for drift checks).
    pub limit: f64,
    pub passed: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub status: Status,
    pub mode: Mode,
    pub window_added_lines: u64,
    pub min_added_lines: u64,
    pub checks: Vec<Check>,
}

impl Verdict {
    /// Process exit code: 0 pass / advisory / insufficient sample, 1 fail in blocking mode.
    pub fn exit_code(&self) -> i32 {
        match (self.status, self.mode) {
            (Status::Fail, Mode::Blocking) => 1,
            _ => 0,
        }
    }

    pub fn failed_checks(&self) -> impl Iterator<Item = &Check> {
        self.checks.iter().filter(|c| !c.passed)
    }
}

fn pct(x: f64) -> String {
    format!("{:.1}%", x * 100.0)
}

/// Evaluate `window` against `baseline` under `policy`. `baseline` may be `None` (no history
/// outside the window yet); drift checks are then skipped and only absolute caps apply.
pub fn evaluate(policy: &Policy, baseline: Option<&Aggregate>, window: &Aggregate) -> Verdict {
    let mut v = Verdict {
        status: Status::Pass,
        mode: policy.gate.mode,
        window_added_lines: window.counts.added,
        min_added_lines: policy.gate.min_added_lines,
        checks: Vec::new(),
    };
    if window.counts.added < policy.gate.min_added_lines {
        v.status = Status::InsufficientSample;
        return v;
    }
    let w = window.ratios;
    let b = baseline.map(|a| a.ratios).unwrap_or_default();

    // Absolute caps.
    let mut cap = |signal: &'static str, observed: Option<f64>, limit: Option<f64>, floor: bool| {
        if let (Some(obs), Some(lim)) = (observed, limit) {
            let passed = if floor { obs >= lim } else { obs <= lim };
            let rel = if floor { "below floor" } else { "above cap" };
            v.checks.push(Check {
                signal: signal.to_string(),
                kind: CheckKind::Absolute,
                observed: obs,
                baseline: None,
                limit: lim,
                passed,
                message: if passed {
                    format!("{signal} {} within cap {}", pct(obs), pct(lim))
                } else {
                    format!("{signal} {} is {rel} {}", pct(obs), pct(lim))
                },
            });
        }
    };
    cap(
        "copy_paste",
        w.copy_paste,
        policy.thresholds.copy_paste_max,
        false,
    );
    cap(
        "dup_block",
        w.dup_block,
        policy.thresholds.dup_block_max,
        false,
    );
    cap("churn", w.churn, policy.thresholds.churn_max, false);
    cap("refactor", w.refactor, policy.thresholds.refactor_min, true);

    // Drift from the repository's own baseline.
    if baseline.is_some() {
        let mut drift = |signal: &'static str,
                         observed: Option<f64>,
                         base: Option<f64>,
                         delta: Option<f64>,
                         drop: bool| {
            if let (Some(obs), Some(base), Some(d)) = (observed, base, delta) {
                let limit = if drop { base - d } else { base + d };
                let passed = if drop { obs >= limit } else { obs <= limit };
                v.checks.push(Check {
                    signal: signal.to_string(),
                    kind: CheckKind::Drift,
                    observed: obs,
                    baseline: Some(base),
                    limit,
                    passed,
                    message: if passed {
                        format!(
                            "{signal} {} vs baseline {} (allowed {}{})",
                            pct(obs),
                            pct(base),
                            if drop { "-" } else { "+" },
                            pct(d)
                        )
                    } else if drop {
                        format!(
                            "{signal} fell to {} from baseline {} (allowed drop {})",
                            pct(obs),
                            pct(base),
                            pct(d)
                        )
                    } else {
                        format!(
                            "{signal} rose to {} from baseline {} (allowed rise {})",
                            pct(obs),
                            pct(base),
                            pct(d)
                        )
                    },
                });
            }
        };
        drift(
            "copy_paste",
            w.copy_paste,
            b.copy_paste,
            policy.drift.copy_paste_max_rise,
            false,
        );
        drift(
            "dup_block",
            w.dup_block,
            b.dup_block,
            policy.drift.dup_block_max_rise,
            false,
        );
        drift(
            "churn",
            w.churn,
            b.churn,
            policy.drift.churn_max_rise,
            false,
        );
        drift(
            "refactor",
            w.refactor,
            b.refactor,
            policy.drift.refactor_max_drop,
            true,
        );
    }

    if v.checks.iter().any(|c| !c.passed) {
        v.status = Status::Fail;
    }
    v
}

/// Convenience for callers holding only ratios (the control plane re-evaluates ingested
/// windows the same way the CLI did).
pub fn evaluate_ratios(
    policy: &Policy,
    baseline: Option<Ratios>,
    window: Ratios,
    window_added: u64,
) -> Verdict {
    let mk = |r: Ratios, added: u64| Aggregate {
        counts: crate::signals::Counts {
            added,
            ..Default::default()
        },
        ratios: r,
        ..Default::default()
    };
    let b = baseline.map(|r| mk(r, 1));
    evaluate(policy, b.as_ref(), &mk(window, window_added))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::Counts;

    fn agg(added: u64, pasted: u64, moved: u64) -> Aggregate {
        let counts = Counts {
            added,
            copy_pasted: pasted,
            moved,
            ..Default::default()
        };
        Aggregate {
            counts,
            ratios: Ratios::of(&counts),
            ..Default::default()
        }
    }

    #[test]
    fn small_windows_are_insufficient_and_pass() {
        let v = evaluate(
            &Policy::default(),
            Some(&agg(1000, 10, 100)),
            &agg(50, 50, 0),
        );
        assert_eq!(v.status, Status::InsufficientSample);
        assert_eq!(v.exit_code(), 0);
    }

    #[test]
    fn drift_beyond_delta_fails_in_blocking_mode_only() {
        let base = agg(1000, 100, 100); // 10% paste, 10% refactor
        let window = agg(1000, 200, 20); // 20% paste, 2% refactor
        let mut p = Policy::default();
        let v = evaluate(&p, Some(&base), &window);
        assert_eq!(v.status, Status::Fail);
        let failed: Vec<_> = v.failed_checks().map(|c| c.signal.as_str()).collect();
        assert_eq!(failed, vec!["copy_paste", "refactor"]);
        assert_eq!(v.exit_code(), 1);
        p.gate.mode = Mode::Advisory;
        assert_eq!(evaluate(&p, Some(&base), &window).exit_code(), 0);
    }

    #[test]
    fn absolute_caps_apply_without_a_baseline() {
        let mut p = Policy::default();
        p.thresholds.copy_paste_max = Some(0.15);
        let v = evaluate(&p, None, &agg(1000, 200, 0));
        assert_eq!(v.status, Status::Fail);
        assert!(v.checks.iter().all(|c| c.kind == CheckKind::Absolute));
        let v = evaluate(&p, None, &agg(1000, 100, 0));
        assert_eq!(v.status, Status::Pass);
    }

    #[test]
    fn policy_parses_from_toml_with_partial_sections() {
        let text =
            "[gate]\nwindow_days = 30\nmode = \"advisory\"\n[drift]\ncopy_paste_max_rise = 0.1\n";
        let p: Policy = toml::from_str(text).unwrap();
        assert_eq!(p.gate.window_days, 30);
        assert_eq!(p.gate.mode, Mode::Advisory);
        assert_eq!(p.drift.copy_paste_max_rise, Some(0.1));
        assert_eq!(p.drift.dup_block_max_rise, Some(0.05));
        assert_eq!(p.gate.min_added_lines, 200);
    }
}
