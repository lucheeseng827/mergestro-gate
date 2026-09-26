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
    /// Wall-clock limit, in seconds, on the Rust mutation run (cargo-mutants,
    /// including its first build). When it is reached the run is stopped, every
    /// mutant that finished counts as usual, and the rest are reported as *not
    /// tested* — never as caught or surviving. `None` (default) = no limit.
    pub budget_secs: Option<u64>,
    /// Block when the budget left mutants untested. Off by default: running out
    /// of time is reported as a warning, since nothing untested was shown to be
    /// wrong.
    pub block_on_budget: bool,
    /// Mutate the checkout itself (`cargo-mutants --in-place`) instead of a
    /// scratch copy. The copy starts with no `target/`, so every run pays a cold
    /// build even when the pre-flight has just built the same tree; in place,
    /// the mutants reuse that build (and a cached `target/`). cargo-mutants then
    /// runs one mutant at a time — `jobs` is ignored — and edits files in the
    /// checkout while it works, restoring each after its test. For CI checkouts,
    /// not a working tree you are editing.
    pub in_place: bool,
    /// Run one shard of the Rust mutants: `"k/n"`, 1-based. Kept mutants are
    /// dealt round-robin in listing order, so every shard sees the same split;
    /// the other engines run on shard 1 only. Combine the shards' `--format
    /// json` reports with `slop-gate merge-reports`. `None` = the whole run.
    pub shard: Option<String>,
    /// How many times the suite is run in the determinism pre-flight. Default 1:
    /// it proves the suite green, which is what the mutation needs. Raise it to 2+
    /// to also catch a flaky suite (a pass/fail flip between runs) — each extra run
    /// costs one full suite before the first mutant.
    pub preflight_runs: u32,
    /// Skip the pre-flight entirely (e.g. CI already proved the suite green).
    pub skip_preflight: bool,
    /// Test command + args used by the pre-flight (program first).
    pub test_command: Vec<String>,
    /// In a workspace, mutation is scoped to the changed crate(s) via
    /// `--package`, and by default so are the *tests* each mutant runs: only the
    /// changed crate's own. That is the fast path. Set `false` (CLI
    /// `--test-workspace`) to run the whole workspace's tests against each mutant,
    /// so a mutant caught only by a downstream crate's tests is still caught —
    /// with the default, that catch shows as a survivor. On by default since
    /// 0.6.0 (it was off in 0.5.x). No field-level `serde(default)`: an omitted
    /// key must take `Config::default()`'s `true`, not `bool::default()`.
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
    /// (`slop` | `security` | `convention` | `docs` | `all`) or a specific
    /// **rule id** (e.g. `hardcoded-secret`, `unknown-crate-import`,
    /// `docs-stale-config`). Empty (the default) leaves every pattern lane
    /// advisory — it scores and reports, never gates.
    #[serde(default)]
    pub block_on_pattern: Vec<String>,

    // ── MCP lane: gate a first-party MCP server ──────────────────────────────
    /// First-party MCP servers this repository ships. Empty (the default) means
    /// the lane never runs; declaring one opts the repo in, and from then on a
    /// PR that touches that server is probed before it can merge.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerTarget>,
    /// How strict the MCP lane is: `never` (report only), `critical` (default —
    /// block on a Critical failure, the tier that would deny the server
    /// admission), `unproven` (also block when a Critical check could not be
    /// run), or `any` (block on any failure).
    ///
    /// This is passed straight to the prober, which owns the check catalog and
    /// its severities — the gate never carries its own copy of which ids are
    /// Critical. A skipped check never blocks at any setting.
    #[serde(default = "default_mcp_fail_on")]
    pub mcp_fail_on: String,
    /// The prober binary the MCP lane drives. A bare name is resolved on PATH.
    #[serde(default = "default_specprobe_bin")]
    pub specprobe_bin: String,

    // ── Turnover lane (turnover): longitudinal maintainability gate ───────
    /// The `turnover:` block — baseline path, whether drift blocks, and the
    /// turnover policy itself (`gate` / `thresholds` / `drift` / `signals` /
    /// `churn` / `walk`, the same keys as `turnover.toml`).
    pub turnover: crate::turnover_lane::TurnoverConfig,
}

/// One first-party MCP server the gate probes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerTarget {
    /// Label used in the report and the PR comment.
    pub name: String,
    /// Repo-relative path prefixes whose change puts this server in scope. The
    /// lane is expensive, so it only runs when the diff touches one of these;
    /// matching is per path segment, so `servers/acme` does not claim
    /// `servers/acme-unrelated`.
    pub paths: Vec<String>,
    /// Repo-relative working directory for the build and the probe. Defaults to
    /// the repository root. Must stay inside the checkout — a PR can edit this
    /// file, and the lane must not become a way to run commands outside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Optional command run before probing (program first), e.g.
    /// `["npm", "run", "build"]`. A failing build blocks: the gate cannot
    /// certify a server it could not produce.
    #[serde(default)]
    pub build: Vec<String>,
    /// The server command the prober spawns (program first). Arguments are
    /// passed as a JSON array, never a shell string, so a path with a space in
    /// it stays one argument.
    pub command: Vec<String>,
    /// The MCP revision to probe against. This also selects which checks run:
    /// `2026-07-28` or later probes the stateless lane, anything earlier the
    /// session lane. Checks outside the era skip rather than failing.
    #[serde(default = "default_spec_version")]
    pub spec_version: String,
    /// Per-exchange deadline in seconds. A hang is a failure, so this bounds
    /// the whole probe.
    #[serde(default = "default_mcp_timeout_secs")]
    pub timeout_secs: u64,
    /// Name of a tool on this server whose call raises an elicitation. Without
    /// it the elicitation check skips — the name cannot be discovered from the
    /// wire, and guessing one would score an `unknown tool` error as a pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elicit_tool: Option<String>,
}

/// The accepted `mcp_fail_on` values. Kept as strings rather than an enum
/// because the value is passed through to the prober verbatim, and the prober
/// is the component that defines them.
pub const MCP_FAIL_ON: [&str; 4] = ["never", "critical", "unproven", "any"];

fn default_mcp_fail_on() -> String {
    "critical".to_string()
}

fn default_specprobe_bin() -> String {
    "specprobe".to_string()
}

fn default_spec_version() -> String {
    "2025-11-25".to_string()
}

fn default_mcp_timeout_secs() -> u64 {
    20
}

/// The recognised pattern-lane names for `block_on_pattern` (besides rule ids).
pub const PATTERN_LANES: [&str; 6] = [
    "slop",
    "security",
    "convention",
    "docs",
    "weakened-tests",
    "all",
];

impl Default for Config {
    fn default() -> Self {
        Self {
            repo: PathBuf::from("."),
            base_ref: "origin/main".to_string(),
            head_ref: "HEAD".to_string(),
            jobs: default_jobs(),
            timeout_secs: 60,
            max_mutants_per_function: 5,
            budget_secs: None,
            block_on_budget: false,
            in_place: false,
            shard: None,
            preflight_runs: 1,
            skip_preflight: false,
            test_command: vec![
                "cargo".to_string(),
                "test".to_string(),
                "--quiet".to_string(),
            ],
            test_changed_package_only: true,
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
            mcp_servers: Vec::new(),
            mcp_fail_on: default_mcp_fail_on(),
            specprobe_bin: default_specprobe_bin(),
            turnover: crate::turnover_lane::TurnoverConfig::default(),
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

    /// `shard` as `(k, n)`, 1-based; `None` when unset or malformed.
    pub fn shard_index(&self) -> Option<(usize, usize)> {
        let (k, n) = self.shard.as_deref()?.split_once('/')?;
        let (k, n): (usize, usize) = (k.trim().parse().ok()?, n.trim().parse().ok()?);
        (1..=n).contains(&k).then_some((k, n))
    }

    /// Reject nonsensical values early, before any subprocess runs.
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.jobs >= 1, "jobs must be >= 1");
        anyhow::ensure!(self.timeout_secs >= 1, "timeout_secs must be >= 1");
        anyhow::ensure!(
            self.budget_secs != Some(0),
            "budget_secs must be >= 1 (omit it for no budget)"
        );
        if let Some(s) = &self.shard {
            anyhow::ensure!(
                self.shard_index().is_some(),
                "shard `{s}` must be `k/n` with 1 <= k <= n, e.g. `2/4`"
            );
        }
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

        // MCP lane. Every failure here is a misconfiguration that would
        // otherwise surface as a probe of the wrong thing, or of nothing.
        anyhow::ensure!(
            MCP_FAIL_ON.contains(&self.mcp_fail_on.trim()),
            "invalid mcp_fail_on `{}` (expected one of {MCP_FAIL_ON:?})",
            self.mcp_fail_on
        );
        anyhow::ensure!(
            !self.specprobe_bin.trim().is_empty(),
            "specprobe_bin must not be empty"
        );
        let mut seen: Vec<&str> = Vec::new();
        for t in &self.mcp_servers {
            let name = t.name.trim();
            anyhow::ensure!(!name.is_empty(), "every mcp_servers entry needs a name");
            anyhow::ensure!(
                !seen.contains(&name),
                "duplicate mcp_servers name `{name}` — names label the report, so they must be unique"
            );
            seen.push(name);
            anyhow::ensure!(
                !t.paths.is_empty(),
                "mcp_servers `{name}` needs at least one path — without one the lane could \
                 never tell whether a diff touched it, so it would never run"
            );
            for p in &t.paths {
                anyhow::ensure!(
                    crate::mcp_gate::is_safe_relative(p.trim_matches('/')),
                    "mcp_servers `{name}` path `{p}` must be repo-relative with no `..`"
                );
            }
            anyhow::ensure!(
                !t.command.is_empty(),
                "mcp_servers `{name}` needs a command (the server to spawn)"
            );
            if let Some(dir) = &t.dir {
                anyhow::ensure!(
                    crate::mcp_gate::is_safe_relative(dir),
                    "mcp_servers `{name}` dir `{dir}` must be repo-relative with no `..` — \
                     this file is editable by a pull request, so the lane stays in the checkout"
                );
            }
            anyhow::ensure!(
                t.timeout_secs >= 1,
                "mcp_servers `{name}` timeout_secs must be >= 1"
            );
            anyhow::ensure!(
                !t.spec_version.trim().is_empty(),
                "mcp_servers `{name}` spec_version must not be empty"
            );
        }
        // Same reasoning as `metrics_url`: the turnover baseline service is authenticated with
        // the METRICS_TOKEN bearer, so the URL a PR-editable config names must be HTTPS with a
        // host, or the tenant token goes out in cleartext.
        if let Some(url) = &self.turnover.baseline_url {
            let host = url
                .trim()
                .trim_end_matches('/')
                .strip_prefix("https://")
                .unwrap_or("");
            anyhow::ensure!(
                !host.is_empty()
                    && !host.starts_with('/')
                    && !host.starts_with('?')
                    && !host.starts_with('#'),
                "turnover.baseline_url must be an https:// URL with a host (got `{url}`)"
            );
            // This is the plane's *base*: the client appends `/v1/turnover/baseline/<repo>`
            // itself, and the plane serves that route at its own root. A base carrying a path
            // therefore requests somewhere that does not exist — and fails silently, which is
            // why this is a validation error rather than a runtime surprise:
            // `turnover_gate::remote::fetch` maps the resulting 404 onto `Ok(false)`, meaning
            // "the plane holds no baseline", so the lane would report `not measured` forever,
            // indistinguishable from a repository nobody has seeded. The likely typo is the
            // telemetry URL, which ends in `/v1/ingest`.
            anyhow::ensure!(
                !host.contains('/'),
                "turnover.baseline_url must be the plane's base URL with no path — the client \
                 appends /v1/turnover/baseline/<repo> itself (got `{url}`)"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shard_is_k_of_n_with_k_in_range() {
        let with = |s: &str| Config {
            shard: Some(s.into()),
            ..Config::default()
        };
        assert_eq!(with("2/4").shard_index(), Some((2, 4)));
        assert!(with("1/1").validate().is_ok());
        for bad in ["0/4", "5/4", "2", "a/b", "2/0", "/4"] {
            assert!(with(bad).validate().is_err(), "should refuse `{bad}`");
        }
    }

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
    fn fast_path_defaults_survive_yaml() {
        // 0.6.0 defaults: one pre-flight run, tests scoped to the changed crate.
        // A YAML file that does not mention them must keep them — a field-level
        // `serde(default)` would silently turn the scope back to `false`.
        let cfg: Config = serde_yaml::from_str("base_ref: develop\n").unwrap();
        assert_eq!(cfg.preflight_runs, 1);
        assert!(cfg.test_changed_package_only);
        // And the file can still widen the tests to the whole workspace.
        let cfg: Config = serde_yaml::from_str("test_changed_package_only: false\n").unwrap();
        assert!(!cfg.test_changed_package_only);
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

    fn mcp_target() -> McpServerTarget {
        McpServerTarget {
            name: "acme".into(),
            paths: vec!["servers/acme".into()],
            dir: None,
            build: vec![],
            command: vec!["node".into(), "server.js".into()],
            spec_version: "2025-11-25".into(),
            timeout_secs: 20,
            elicit_tool: None,
        }
    }

    fn cfg_with_mcp(t: McpServerTarget) -> Config {
        Config {
            mcp_servers: vec![t],
            ..Config::default()
        }
    }

    #[test]
    fn mcp_defaults_gate_on_critical_and_resolve_the_prober_on_path() {
        // Declaring a server IS the opt-in, so the lane gates from the first run
        // rather than needing a second flag nobody sets.
        let cfg = Config::default();
        assert_eq!(cfg.mcp_fail_on, "critical");
        assert_eq!(cfg.specprobe_bin, "specprobe");
        assert!(cfg.mcp_servers.is_empty());
    }

    #[test]
    fn mcp_server_yaml_fills_its_defaults() {
        let yaml = "mcp_servers:\n  - name: acme\n    paths: [servers/acme]\n    command: [node, server.js]\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        cfg.validate()
            .expect("a minimal server entry must validate");
        let t = &cfg.mcp_servers[0];
        assert_eq!(t.spec_version, "2025-11-25");
        assert_eq!(t.timeout_secs, 20);
        assert!(t.build.is_empty());
        assert_eq!(t.dir, None);
    }

    #[test]
    fn validate_rejects_an_unknown_mcp_threshold() {
        let cfg = Config {
            mcp_fail_on: "warn".into(),
            ..Config::default()
        };
        let e = cfg.validate().unwrap_err().to_string();
        assert!(e.contains("warn") && e.contains("critical"), "{e}");
    }

    #[test]
    fn validate_rejects_a_server_that_could_never_be_triggered() {
        // No paths means the lane can never tell whether a diff touched this
        // server — it would sit in the config looking like coverage and never
        // run once.
        let mut t = mcp_target();
        t.paths.clear();
        let e = cfg_with_mcp(t).validate().unwrap_err().to_string();
        assert!(e.contains("at least one path"), "{e}");
    }

    #[test]
    fn validate_rejects_a_server_with_no_command() {
        let mut t = mcp_target();
        t.command.clear();
        let e = cfg_with_mcp(t).validate().unwrap_err().to_string();
        assert!(e.contains("needs a command"), "{e}");
    }

    #[test]
    fn validate_keeps_the_lane_inside_the_checkout() {
        // This file is editable by a pull request. A `dir` of `../../` would
        // turn the lane into a way to run a build command anywhere on the runner.
        for bad in ["../elsewhere", "/etc", "servers/../../elsewhere"] {
            let mut t = mcp_target();
            t.dir = Some(bad.into());
            let e = cfg_with_mcp(t).validate().unwrap_err().to_string();
            assert!(e.contains("repo-relative"), "{bad}: {e}");
        }
        let mut ok = mcp_target();
        ok.dir = Some("servers/acme".into());
        assert!(cfg_with_mcp(ok).validate().is_ok());
    }

    #[test]
    fn validate_rejects_duplicate_server_names() {
        let cfg = Config {
            mcp_servers: vec![mcp_target(), mcp_target()],
            ..Config::default()
        };
        let e = cfg.validate().unwrap_err().to_string();
        assert!(e.contains("duplicate"), "{e}");
    }

    #[test]
    fn validate_rejects_a_zero_probe_timeout() {
        let mut t = mcp_target();
        t.timeout_secs = 0;
        let e = cfg_with_mcp(t).validate().unwrap_err().to_string();
        assert!(e.contains("timeout_secs"), "{e}");
    }
}
