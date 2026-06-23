// SPDX-License-Identifier: Apache-2.0
//! Gate configuration: thresholds, caps, gating flags and paths from one file,
//! with sensible defaults so the gate runs with zero config.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::severity::Severity;

/// All knobs the gate exposes. Every field has a default, so a bare
/// `Config::default()` is runnable; a YAML file or CLI flags override fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Path to the repository under test.
    pub repo: PathBuf,
    /// Base ref the diff is taken against (PR merge-base semantics).
    pub base_ref: String,
    /// Head ref being gated.
    pub head_ref: String,
    /// Parallel mutant jobs (`cargo-mutants --jobs`).
    pub jobs: usize,
    /// Per-mutant test timeout in seconds (`cargo-mutants --timeout`).
    pub timeout_secs: u64,
    /// Hard cap on mutants tested per function — the core runtime lever.
    pub max_mutants_per_function: usize,
    /// How many times the suite is run in the determinism pre-flight.
    pub preflight_runs: u32,
    /// Skip the pre-flight entirely (e.g. CI already proved the suite green).
    pub skip_preflight: bool,
    /// Test command + args used by the pre-flight (program first).
    pub test_command: Vec<String>,
    /// In a workspace, mutation is scoped to the changed crate(s) via
    /// `--package`. By default the *tests* still run across the whole workspace
    /// (`--test-workspace`), so a mutant caught only by a downstream crate's
    /// tests is still caught — no false survivors. Set `true` to narrow tests to
    /// the changed package only: faster, but a downstream-only catch then shows
    /// as a survivor. Off by default (correctness over speed).
    #[serde(default)]
    pub test_changed_package_only: bool,
    /// Test runner cargo-mutants drives: `"cargo"` (default) or `"nextest"`.
    /// `nextest` runs each test in its own process, highly parallel — often a
    /// 2–3× faster test phase. Requires `cargo-nextest` installed.
    #[serde(default = "default_test_tool")]
    pub test_tool: String,

    // ── Phase 2: gating ─────────────────────────────────────────────────────
    /// Block (fail the build) when survivors exceed `max_survivors`.
    pub block_on_survivors: bool,
    /// Survivors tolerated before blocking. `0` means any survivor blocks.
    pub max_survivors: usize,
    /// Run the static zero-assertion test pre-check.
    pub check_zero_assertion_tests: bool,
    /// Also block when zero-assertion tests are found (advisory by default —
    /// it's a heuristic, so it scores but doesn't gate unless turned on).
    pub block_on_zero_assertion_tests: bool,

    // ── Phase 3: validation telemetry ───────────────────────────────────────
    /// Append a JSON-Lines run record to this file (one object per run).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics_file: Option<PathBuf>,
    /// POST the run record to this telemetry endpoint (best-effort).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics_url: Option<String>,

    // ── Phase 4: rollout (severity ranking + debt-delta budget) ──────────────
    /// Block when any survivor's severity is at or above this tier, regardless
    /// of `max_survivors`. `None` (the default) leaves severity advisory — it
    /// still orders the report, but doesn't gate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_on_severity: Option<Severity>,
    /// Per-PR structural-debt budget (net decision points + duplication +
    /// coupling added). The debt-delta is always reported; it only *gates* when
    /// `block_on_debt` is set. `0` disables the burn-rate framing.
    pub debt_budget: i64,
    /// Block when the debt-delta exceeds `debt_budget` (advisory by default).
    pub block_on_debt: bool,

    // ── Phase 4 (cont.): Python adapter (advisory PoC) ──────────────────────
    /// Determinism pre-flight + cosmic-ray test command for Python changes
    /// (program first). cosmic-ray runs the joined string per mutant.
    pub python_test_command: Vec<String>,

    // ── Track B: pattern lanes (opt-in gating) ───────────────────────────────
    /// Block when a pattern lane flags something. Each entry is a **lane name**
    /// (`slop` | `security` | `convention` | `all`) or a specific **rule id**
    /// (e.g. `hardcoded-secret`, `unknown-crate-import`). Empty (the default)
    /// leaves every pattern lane advisory — it scores and reports, never gates.
    #[serde(default)]
    pub block_on_pattern: Vec<String>,
}

/// The recognised pattern-lane names for `block_on_pattern` (besides rule ids).
pub const PATTERN_LANES: [&str; 4] = ["slop", "security", "convention", "all"];

impl Default for Config {
    fn default() -> Self {
        Self {
            repo: PathBuf::from("."),
            base_ref: "origin/main".to_string(),
            head_ref: "HEAD".to_string(),
            jobs: default_jobs(),
            timeout_secs: 60,
            max_mutants_per_function: 5,
            preflight_runs: 2,
            skip_preflight: false,
            test_command: vec![
                "cargo".to_string(),
                "test".to_string(),
                "--quiet".to_string(),
            ],
            test_changed_package_only: false,
            test_tool: default_test_tool(),
            block_on_survivors: true,
            max_survivors: 0,
            check_zero_assertion_tests: true,
            block_on_zero_assertion_tests: false,
            metrics_file: None,
            metrics_url: None,
            block_on_severity: None,
            debt_budget: 25,
            block_on_debt: false,
            python_test_command: vec![
                "python3".to_string(),
                "-m".to_string(),
                "pytest".to_string(),
                "-q".to_string(),
            ],
            block_on_pattern: Vec::new(),
        }
    }
}

/// Default test runner for cargo-mutants.
fn default_test_tool() -> String {
    "cargo".to_string()
}

/// Default parallelism: available cores, clamped so CI runners aren't
/// oversubscribed. Falls back to 1 if the count can't be determined.
fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(1)
}

impl Config {
    /// Load a YAML config from `path`, layering it over the defaults.
    pub fn from_yaml_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let cfg: Config = serde_yaml::from_str(&text)
            .with_context(|| format!("parsing YAML config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reject nonsensical values early, before any subprocess runs.
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.jobs >= 1, "jobs must be >= 1");
        anyhow::ensure!(self.timeout_secs >= 1, "timeout_secs must be >= 1");
        anyhow::ensure!(
            self.max_mutants_per_function >= 1,
            "max_mutants_per_function must be >= 1"
        );
        anyhow::ensure!(self.preflight_runs >= 1, "preflight_runs must be >= 1");
        anyhow::ensure!(
            !self.test_command.is_empty(),
            "test_command must not be empty"
        );
        anyhow::ensure!(
            !self.python_test_command.is_empty(),
            "python_test_command must not be empty"
        );
        // Telemetry can carry a Bearer token (METRICS_TOKEN), so refuse to emit
        // it over anything but HTTPS — a misconfigured or PR-supplied endpoint
        // must not be able to exfiltrate it in plaintext.
        if let Some(url) = &self.metrics_url {
            // Require an https:// URL with a real host — reject `https://`,
            // `https:///path` and `https://?q=1` (empty/missing host).
            let host = url.trim().strip_prefix("https://").unwrap_or("");
            anyhow::ensure!(
                !host.is_empty()
                    && !host.starts_with('/')
                    && !host.starts_with('?')
                    && !host.starts_with('#'),
                "metrics_url must be an https:// URL with a host (got `{url}`)"
            );
        }
        anyhow::ensure!(self.debt_budget >= 0, "debt_budget must be >= 0");
        anyhow::ensure!(
            matches!(self.test_tool.as_str(), "cargo" | "nextest"),
            "test_tool must be `cargo` or `nextest` (got `{}`)",
            self.test_tool
        );
        // Each block-on-pattern target is a known lane or a rule-id-shaped token
        // (lowercase, `a-z0-9-`). This catches typos that would otherwise just
        // never match and silently fail to gate.
        for target in &self.block_on_pattern {
            let t = target.trim();
            let is_lane = PATTERN_LANES.contains(&t);
            let is_rule_shaped = !t.is_empty()
                && t.starts_with(|c: char| c.is_ascii_lowercase())
                && t.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            anyhow::ensure!(
                is_lane || is_rule_shaped,
                "invalid --block-on-pattern `{target}` (expected a lane {PATTERN_LANES:?} or a rule id like `hardcoded-secret`)"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default()
            .validate()
            .expect("defaults must validate");
    }

    #[test]
    fn yaml_overrides_layer_over_defaults() {
        // Only set two fields; everything else should fall back to defaults.
        let yaml = "base_ref: develop\nmax_mutants_per_function: 3\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.base_ref, "develop");
        assert_eq!(cfg.max_mutants_per_function, 3);
        // Untouched field keeps its default.
        assert_eq!(cfg.head_ref, "HEAD");
        assert_eq!(cfg.test_command.first().unwrap(), "cargo");
    }

    #[test]
    fn validate_requires_https_metrics_url() {
        let https = Config {
            metrics_url: Some("https://example.com/ingest".into()),
            ..Config::default()
        };
        assert!(https.validate().is_ok());

        for bad in [
            "http://example.com",
            "https://",
            "https:///ingest",
            "https://?q=1",
            "https://#x",
            "ftp://x",
            "example.com",
        ] {
            let cfg = Config {
                metrics_url: Some(bad.into()),
                ..Config::default()
            };
            assert!(cfg.validate().is_err(), "should reject `{bad}`");
        }
    }

    #[test]
    fn block_on_severity_round_trips_through_yaml() {
        let cfg: Config = serde_yaml::from_str("block_on_severity: critical\n").unwrap();
        assert_eq!(cfg.block_on_severity, Some(Severity::Critical));
        // Default leaves it unset (advisory).
        assert_eq!(Config::default().block_on_severity, None);
    }

    #[test]
    fn validate_rejects_negative_debt_budget() {
        let cfg = Config {
            debt_budget: -1,
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_python_test_command() {
        // An empty command would panic in preflight (`command[0]`), so reject it.
        let cfg = Config {
            python_test_command: vec![],
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_accepts_lanes_and_rule_ids_but_rejects_garbage() {
        for ok in [
            vec!["security".to_string()],
            vec!["all".to_string()],
            vec![
                "hardcoded-secret".to_string(),
                "unknown-crate-import".to_string(),
            ],
        ] {
            let cfg = Config {
                block_on_pattern: ok.clone(),
                ..Config::default()
            };
            assert!(cfg.validate().is_ok(), "should accept {ok:?}");
        }
        for bad in ["Security", "has space", "UPPER", "", "rule_with_underscore"] {
            let cfg = Config {
                block_on_pattern: vec![bad.to_string()],
                ..Config::default()
            };
            assert!(cfg.validate().is_err(), "should reject `{bad}`");
        }
    }

    #[test]
    fn validate_test_tool_accepts_cargo_nextest_rejects_other() {
        for ok in ["cargo", "nextest"] {
            let cfg = Config {
                test_tool: ok.into(),
                ..Config::default()
            };
            assert!(cfg.validate().is_ok(), "should accept {ok}");
        }
        let cfg = Config {
            test_tool: "pytest".into(),
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_cap() {
        let cfg = Config {
            max_mutants_per_function: 0,
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
    }
}
