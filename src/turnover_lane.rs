// SPDX-License-Identifier: Apache-2.0
//! The turnover lane — the longitudinal maintainability gate, in-process.
//!
//! Every other lane reads the diff. This one reads the repository's *history*: it refreshes
//! the turnover baseline (one classified row per commit, kept in `.turnover/baseline.json`)
//! with the commits this change adds, then judges those commits' copy/paste, duplicated-
//! block, refactor and churn ratios against the repository's own baseline under the drift
//! policy. The verdict, the numbers and the Markdown section all come from `turnover-gate`,
//! the same library the standalone `turnover` binary runs, so a change fails for the same
//! reason on both paths.
//!
//! Advisory by default (`turnover.block_on_drift: false`), like the debt-delta: the drift
//! limits are the repository's own trajectory and a team turns the gate on once it has read
//! a few reports. A missing baseline is a **skipped** lane with a hint, never a block — the
//! gate cannot certify what it has not measured, and a repository that has not built a
//! baseline yet is the common first run, not a failure.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use turnover_core::policy::{Status, Verdict};
use turnover_core::window::{Aggregate, Ratios};
use turnover_gate::{run_gate, GateError, GateRequest, Scope};

use crate::config::Config;

/// The `turnover:` block of the gate config. The policy itself is turnover's own TOML
/// schema (`gate` / `thresholds` / `drift` / `signals` / `churn` / `walk`), flattened here so
/// one YAML file configures every lane.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TurnoverConfig {
    /// Run the lane at all.
    pub enabled: bool,
    /// The baseline file (relative to the repo, or absolute).
    pub baseline: PathBuf,
    /// Turn a drifted verdict into a block reason. Advisory otherwise.
    pub block_on_drift: bool,
    /// Walk the commits newer than the stored baseline head before judging. Off only when
    /// something else already refreshed the file this run.
    pub refresh: bool,
    /// Persist the refreshed baseline back to disk after the run.
    pub update_baseline: bool,
    /// The Mergestro plane's URL (the same host as `metrics_url`). When set, the lane fetches
    /// the repo's baseline from the plane's baseline service before judging and, with
    /// `update_baseline`, pushes the refreshed file back — so a runner needs neither a cache
    /// nor a full clone.
    ///
    /// Token: `TURNOVER_TOKEN`, else `METRICS_TOKEN` — that order, which is `token_from_env`'s.
    /// Prefer the first: in a repository that also emits telemetry, `METRICS_TOKEN` is already
    /// the ingest token, and reusing it hands the plane's baseline service a credential scoped
    /// for something else. This repository is a live example — see mergestro-gate/docs/OPERATIONS.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_url: Option<String>,
    /// The turnover policy (`turnover.toml` keys, verbatim).
    #[serde(flatten)]
    pub policy: turnover_gate::Config,
}

impl Default for TurnoverConfig {
    fn default() -> Self {
        TurnoverConfig {
            enabled: true,
            baseline: PathBuf::from(".turnover/baseline.json"),
            block_on_drift: false,
            refresh: true,
            update_baseline: false,
            baseline_url: None,
            policy: turnover_gate::Config::default(),
        }
    }
}

/// What the lane measured — the subset of `turnover-gate`'s outcome the report, verdict and
/// telemetry read. Rendered strings are carried so the report never needs turnover's types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TurnoverLane {
    /// `pass` | `fail` | `insufficient_sample` | `skipped`.
    pub status: String,
    /// `blocking` | `advisory` — the policy's mode (the gate's own `block_on_drift` decides
    /// whether a `fail` blocks the build).
    pub mode: String,
    /// Why the lane was skipped, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The commits judged, in words.
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub window_days: u32,
    #[serde(default)]
    pub head_sha: String,
    #[serde(default)]
    pub window: Aggregate,
    #[serde(default)]
    pub baseline: Aggregate,
    #[serde(default)]
    pub has_baseline: bool,
    #[serde(default)]
    pub failed_checks: Vec<String>,
    /// Human sentences for each failed check — the block reasons when the lane gates.
    #[serde(default)]
    pub messages: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// The Markdown section for the PR comment (no top-level heading).
    #[serde(default)]
    pub markdown: String,
    /// The job-log report.
    #[serde(default)]
    pub text: String,
}

impl TurnoverLane {
    fn skipped(reason: String) -> Self {
        TurnoverLane {
            status: "skipped".to_string(),
            mode: "advisory".to_string(),
            reason: Some(reason),
            scope: String::new(),
            window_days: 0,
            head_sha: String::new(),
            window: Aggregate::default(),
            baseline: Aggregate::default(),
            has_baseline: false,
            failed_checks: Vec::new(),
            messages: Vec::new(),
            verdict: None,
            markdown: String::new(),
            text: String::new(),
        }
    }

    pub fn is_fail(&self) -> bool {
        self.status == "fail"
    }

    pub fn ratios(&self) -> Ratios {
        self.window.ratios
    }

    pub fn baseline_ratios(&self) -> Ratios {
        self.baseline.ratios
    }
}

/// Run the lane for the gate's diff range: the commits reachable from `head_ref` but not
/// from `base_ref`. Never returns an error — a tool failure is a skipped lane with the
/// reason in the report, because the mutation gate's verdict must not depend on whether a
/// baseline file happened to be present.
/// The baseline file the lane reads and refreshes: `turnover.baseline` as given when
/// absolute, otherwise relative to the repository root (never the process cwd).
fn baseline_path(cfg: &Config) -> PathBuf {
    let t = &cfg.turnover;
    if t.baseline.is_absolute() {
        t.baseline.clone()
    } else {
        cfg.repo.join(&t.baseline)
    }
}

/// What the plane's baseline service did for this run.
///
/// The missing-baseline hint needs this to name a remedy that will actually work. One fixed
/// string used to serve three states whose fixes have nothing in common — no plane configured, a
/// plane holding no baseline for this repo yet, and a plane that could not be reached — and it
/// told all three to build the file locally and "commit or cache" it. That is wrong twice over
/// in a repository wired to a plane: `.turnover/` is gitignored, so there is nothing to commit,
/// and the plane keys each baseline by `GITHUB_REPOSITORY`, so a laptop's walk writes it under
/// the checkout's directory name — a key the gate never reads. An operator following the old
/// hint would do the work and still be told "not measured".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaneFetch {
    /// No `baseline_url`, so the lane only ever looked on disk.
    NotConfigured,
    /// The plane answered and had a baseline for this repo.
    Fetched,
    /// The plane answered that it holds none for this repo. `remote::fetch` maps its 404 onto
    /// `Ok(false)`, which is why this state is otherwise indistinguishable from success.
    Empty,
    /// The plane could not be reached, or refused the request. `run` has already put the error
    /// on stderr; this carries the fact into the report, where "not measured" would otherwise
    /// read as "nothing configured".
    Unreachable,
}

/// The hint a skipped lane carries when there is no baseline to judge against.
///
/// Split out of `run` so each wiring's advice can be asserted without a repository, a plane or a
/// history walk.
fn missing_baseline_hint(
    plane: PlaneFetch,
    url: Option<&str>,
    plane_repo: &str,
    path: &Path,
) -> String {
    // `url` is Some whenever `plane` is anything but NotConfigured, but say "the configured URL"
    // rather than panic: a hint is the wrong place to take a process down.
    let url = url.unwrap_or("the configured URL");
    match plane {
        PlaneFetch::NotConfigured => format!(
            "no turnover baseline at {} and no plane configured — either run `slop-gate baseline` \
             once with full history to build one on disk, or set `turnover.baseline_url` (the \
             MERGESTRO_PLANE_URL variable in CI) so the lane fetches it from the Mergestro plane",
            path.display()
        ),
        PlaneFetch::Empty => format!(
            "the plane at {url} holds no turnover baseline for {plane_repo} yet — run the \
             `turnover baseline (seed)` workflow, which walks full history and pushes it there. \
             Seeding from a laptop writes it under the wrong repo key"
        ),
        PlaneFetch::Unreachable => format!(
            "the turnover baseline could not be fetched from {url} (see the warning above) and \
             there is none at {} — check the URL is the plane's base with no path, and that \
             TURNOVER_TOKEN is set",
            path.display()
        ),
        // The fetch wrote the file, so the gate should have found it. Not a wiring problem, and
        // saying so keeps the operator from re-checking settings that are already right.
        PlaneFetch::Fetched => format!(
            "the plane's turnover baseline for {plane_repo} was fetched but the gate found none \
             at {} — this is a bug in the gate, not a configuration mistake",
            path.display()
        ),
    }
}

pub fn run(cfg: &Config) -> TurnoverLane {
    let t = &cfg.turnover;
    let baseline_path = baseline_path(cfg);
    let plane_repo = turnover_gate::run::repo_display_name(&cfg.repo);
    let token = turnover_gate::remote::token_from_env().unwrap_or_default();
    let mut plane = PlaneFetch::NotConfigured;
    if let Some(url) = &t.baseline_url {
        plane = match turnover_gate::remote::fetch(url, &token, &plane_repo, &baseline_path) {
            Ok(true) => {
                eprintln!("slop-gate: turnover baseline fetched from {url} for {plane_repo}");
                PlaneFetch::Fetched
            }
            Ok(false) => PlaneFetch::Empty,
            Err(e) => {
                eprintln!("slop-gate: warning: could not fetch the turnover baseline: {e:#}");
                PlaneFetch::Unreachable
            }
        };
    }
    let req = GateRequest {
        repo: cfg.repo.clone(),
        config: t.policy.clone(),
        baseline_path: baseline_path.clone(),
        scope: Scope::BaseRef(cfg.base_ref.clone()),
        refresh: t.refresh,
        update_baseline: t.update_baseline,
        advisory: false,
    };
    let outcome = match run_gate(&req) {
        Ok(o) => o,
        Err(GateError::MissingBaseline(_)) => {
            return TurnoverLane::skipped(missing_baseline_hint(
                plane,
                t.baseline_url.as_deref(),
                &plane_repo,
                &baseline_path,
            ))
        }
        Err(GateError::ConfigMismatch) => {
            return TurnoverLane::skipped(
                "turnover classifier settings differ from the baseline's — rebuild it with `slop-gate baseline --full`".to_string(),
            )
        }
        Err(GateError::Other(e)) => return TurnoverLane::skipped(format!("turnover lane did not run: {e:#}")),
    };
    if let (Some(url), true) = (&t.baseline_url, t.update_baseline) {
        if let Err(e) = turnover_gate::remote::push(url, &token, &plane_repo, &baseline_path) {
            eprintln!("slop-gate: warning: could not push the turnover baseline: {e:#}");
        }
    }
    let status = match outcome.verdict.status {
        Status::Pass => "pass",
        Status::Fail => "fail",
        Status::InsufficientSample => "insufficient_sample",
    };
    TurnoverLane {
        status: status.to_string(),
        mode: outcome.verdict.mode.as_str().to_string(),
        reason: None,
        scope: outcome.scope.clone(),
        window_days: outcome.window_days,
        head_sha: outcome.head_sha.clone(),
        failed_checks: outcome
            .verdict
            .failed_checks()
            .map(|c| c.signal.clone())
            .collect(),
        messages: outcome
            .verdict
            .failed_checks()
            .map(|c| c.message.clone())
            .collect(),
        markdown: turnover_gate::render::markdown(&outcome),
        text: turnover_gate::render::text(&outcome),
        has_baseline: outcome.has_baseline,
        window: outcome.window.clone(),
        baseline: outcome.baseline.clone(),
        verdict: Some(outcome.verdict),
    }
}

/// `slop-gate baseline`: build or refresh the turnover baseline for `cfg.repo`.
pub fn build_baseline(
    cfg: &Config,
    full: bool,
    since_days: Option<u32>,
) -> anyhow::Result<turnover_gate::BaselineSummary> {
    let t = &cfg.turnover;
    let baseline_path = baseline_path(cfg);
    let summary = turnover_gate::build_baseline(&turnover_gate::BaselineRequest {
        repo: cfg.repo.clone(),
        config: t.policy.clone(),
        baseline_path: baseline_path.clone(),
        tip: cfg.head_ref.clone(),
        since_days,
        full,
    })?;
    if let Some(url) = &t.baseline_url {
        let repo = turnover_gate::run::repo_display_name(&cfg.repo);
        let token = turnover_gate::remote::token_from_env().unwrap_or_default();
        // The baseline is already on disk, so a plane problem must not fail the build: the
        // next run simply pushes it again. Same posture as the fetch and push in `run`.
        match turnover_gate::remote::push(url, &token, &repo, &baseline_path) {
            Ok(()) => eprintln!("slop-gate: turnover baseline pushed to {url} for {repo}"),
            Err(e) => eprintln!(
                "slop-gate: warning: could not push the turnover baseline to {url}: {e:#}"
            ),
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_turnover_block_parses_with_flattened_policy_and_defaults() {
        let yaml = "turnover:\n  block_on_drift: true\n  baseline: baselines/api.json\n  gate:\n    window_days: 30\n    min_added_lines: 50\n  drift:\n    copy_paste_max_rise: 0.1\n  signals:\n    block_min_lines: 4\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.turnover.enabled);
        assert!(cfg.turnover.block_on_drift);
        assert_eq!(cfg.turnover.baseline, PathBuf::from("baselines/api.json"));
        assert_eq!(cfg.turnover.policy.policy.gate.window_days, 30);
        assert_eq!(cfg.turnover.policy.policy.gate.min_added_lines, 50);
        assert_eq!(
            cfg.turnover.policy.policy.drift.copy_paste_max_rise,
            Some(0.1)
        );
        assert_eq!(
            cfg.turnover.policy.policy.drift.dup_block_max_rise,
            Some(0.05)
        );
        assert_eq!(cfg.turnover.policy.signals.block_min_lines, 4);
        assert_eq!(cfg.turnover.policy.churn.horizon_days, 14);
        // Round-trips through YAML with the flattened block intact.
        let text = serde_yaml::to_string(&cfg).unwrap();
        let back: Config = serde_yaml::from_str(&text).unwrap();
        assert_eq!(back.turnover.policy.signals.block_min_lines, 4);
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t.io")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t.io")
            .output()
            .expect("git available");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn commit_file(dir: &std::path::Path, name: &str, body: &str, msg: &str) {
        std::fs::write(dir.join(name), body).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "--no-gpg-sign", "-m", msg]);
    }

    /// Honest code: four statements per function drawn from a pool of pairwise shape-distinct
    /// lines (each a longer arithmetic chain), rotated by `i`, so nothing repeats even after
    /// the classifier abstracts identifiers and literals (rename-insensitive block matching).
    fn func(i: usize) -> String {
        let stmt = |k: usize| {
            let mut line = format!("    let acc_{k} = base_{k}");
            for j in 0..=k {
                line.push_str(&format!(" + items[{j}].price"));
            }
            line.push_str(";\n");
            line
        };
        let mut body = format!("fn honest_{i}(items: &[Item], base_0: u64) -> u64 {{\n");
        for k in i..i + 4 {
            body.push_str(&stmt(k));
        }
        body.push_str(&format!("    acc_{}\n}}\n", i + 3));
        body
    }

    /// The lane end to end inside `pipeline::run`: skipped without a baseline, then a real
    /// verdict on the PR's own commits once `slop-gate baseline` has built one. The fixture is
    /// a Ruby file: no mutation engine claims it, so the pipeline takes its no-engine path and
    /// the turnover lane — which reads history, not the diff, and counts Ruby through the
    /// heuristic mask — is the only lane that measures anything.
    #[test]
    fn lane_runs_inside_the_pipeline_once_a_baseline_exists() {
        use crate::runner::test_support::ScriptedRunner;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        let mut body = String::new();
        for i in 0..6 {
            body.push_str(&func(i));
            commit_file(dir, "lib.rb", &body, &format!("c{i}"));
        }
        // The change under gate: one more honest function.
        body.push_str(&func(99));
        commit_file(dir, "lib.rb", &body, "head");

        let mut cfg = Config {
            repo: dir.to_path_buf(),
            base_ref: "HEAD~1".into(),
            head_ref: "HEAD".into(),
            preflight_runs: 1,
            skip_preflight: true,
            ..Config::default()
        };
        cfg.turnover.policy.policy.gate.min_added_lines = 1;
        cfg.turnover.baseline = dir.join(".turnover/baseline.json");

        // No baseline yet: the lane skips, the gate does not block on it.
        let lane = run(&cfg);
        assert_eq!(lane.status, "skipped");

        let summary = build_baseline(&cfg, false, None).unwrap();
        assert_eq!(summary.total_commits, 7);
        assert!(cfg.turnover.baseline.exists());

        let runner = ScriptedRunner::new();
        let work = tempfile::tempdir().unwrap();
        let report = crate::pipeline::run(&runner, &cfg, work.path()).unwrap();
        let lane = report.turnover.as_ref().expect("lane ran");
        assert_eq!(lane.status, "pass", "{lane:?}");
        assert_eq!(lane.window.commits, 1, "only the PR's commit is judged");
        assert!(lane.window.counts.added >= 4);
        assert!(lane.has_baseline);
        assert!(
            lane.scope
                .contains("commits in HEAD not in HEAD~1 (1 commits)"),
            "{}",
            lane.scope
        );
        assert!(
            lane.markdown.contains("within the drift budget"),
            "{}",
            lane.markdown
        );
        let md = report.render_markdown();
        assert!(
            md.contains("### Maintainability drift (turnover)\n"),
            "{md}"
        );
        assert!(md.contains("| copy/paste |"), "{md}");
        let text = report.render_text();
        assert!(text.contains("turnover:   PASS (blocking)"), "{text}");
    }

    #[test]
    fn baseline_url_must_be_https_with_a_host() {
        let mut cfg = Config::default();
        cfg.turnover.baseline_url = Some("https://plane.example".into());
        cfg.validate().unwrap();
        // A trailing slash is fine: the client trims it before appending its own path.
        cfg.turnover.baseline_url = Some("https://plane.example/".into());
        cfg.validate().unwrap();
        for bad in [
            "http://plane.example",
            "https://",
            "https:///v1",
            "plane.example",
        ] {
            cfg.turnover.baseline_url = Some(bad.into());
            let err = cfg.validate().unwrap_err().to_string();
            assert!(err.contains("turnover.baseline_url"), "{bad}: {err}");
        }
    }

    /// A base carrying a path is the failure that hides: the client appends
    /// `/v1/turnover/baseline/<repo>` to it, the plane serves that route at its root, and
    /// `remote::fetch` reads the resulting 404 as "no baseline stored" — so the lane reports
    /// `not measured` forever rather than complaining. `/v1/ingest` is the likely typo, since
    /// that is what the telemetry URL looks like.
    #[test]
    fn baseline_url_must_not_carry_a_path() {
        let mut cfg = Config::default();
        for bad in [
            "https://plane.example/prefix",
            "https://plane.example/v1/ingest",
            "https://plane.example/v1/turnover/baseline",
            "https://plane.example/prefix/",
        ] {
            cfg.turnover.baseline_url = Some(bad.into());
            let err = cfg.validate().unwrap_err().to_string();
            assert!(
                err.contains("no path"),
                "{bad} should be rejected for carrying a path, got: {err}"
            );
        }
    }

    #[test]
    fn missing_baseline_is_a_skip_with_a_hint_not_a_block() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config {
            repo: tmp.path().to_path_buf(),
            ..Config::default()
        };
        let lane = run(&cfg);
        assert_eq!(lane.status, "skipped");
        assert!(
            lane.reason
                .as_deref()
                .unwrap()
                .contains("slop-gate baseline"),
            "{:?}",
            lane.reason
        );
        assert!(!lane.is_fail());
    }

    /// The hint is the whole value of a skipped lane: the lane reports no number, so the reason
    /// string is the only thing an operator can act on. Each wiring must name the remedy that
    /// works for *it* — a plane that holds nothing is fixed by the seed workflow, never by
    /// building the file on the machine that just asked for it.
    #[test]
    fn each_wiring_gets_the_remedy_that_fixes_it() {
        let path = Path::new("/w/.turnover/baseline.json");
        let url = Some("https://plane.example");

        let unconfigured =
            missing_baseline_hint(PlaneFetch::NotConfigured, None, "owner/repo", path);
        assert!(
            unconfigured.contains("slop-gate baseline") && unconfigured.contains("baseline_url"),
            "with no plane, both routes are open and the hint should offer them: {unconfigured}"
        );

        let empty = missing_baseline_hint(PlaneFetch::Empty, url, "owner/repo", path);
        assert!(
            empty.contains("seed")
                && empty.contains("plane.example")
                && empty.contains("owner/repo"),
            "an empty plane is fixed by seeding it, and the hint must say which repo key: {empty}"
        );

        let unreachable = missing_baseline_hint(PlaneFetch::Unreachable, url, "owner/repo", path);
        assert!(
            unreachable.contains("TURNOVER_TOKEN") && unreachable.contains("no path"),
            "an unreachable plane is a wiring fault, so name the two things that are usually \
             wrong: {unreachable}"
        );

        let bug = missing_baseline_hint(PlaneFetch::Fetched, url, "owner/repo", path);
        assert!(
            bug.contains("bug"),
            "a fetched-but-missing baseline is not the operator's to fix: {bug}"
        );
    }

    /// The advice that sent people to `git add` a gitignored file, and to seed the plane from a
    /// machine whose repo key the gate never reads. Neither belongs in a plane-configured state.
    #[test]
    fn a_configured_plane_is_never_told_to_commit_the_file() {
        let path = Path::new("/w/.turnover/baseline.json");
        let url = Some("https://plane.example");
        for plane in [
            PlaneFetch::Empty,
            PlaneFetch::Unreachable,
            PlaneFetch::Fetched,
        ] {
            let hint = missing_baseline_hint(plane, url, "owner/repo", path);
            assert!(
                !hint.contains("commit"),
                "{plane:?} told the operator to commit a gitignored file: {hint}"
            );
            assert!(
                !hint.contains("cache"),
                "{plane:?} told the operator to cache a file the plane serves: {hint}"
            );
        }
    }

    /// `Ok(false)` from `remote::fetch` is the plane's 404, and it is the state a freshly wired
    /// repository lands in. If it read the same as "nothing configured", the first operator to
    /// set MERGESTRO_PLANE_URL would be told to go set MERGESTRO_PLANE_URL.
    #[test]
    fn an_empty_plane_does_not_read_as_an_unconfigured_one() {
        let path = Path::new("/w/.turnover/baseline.json");
        let empty = missing_baseline_hint(
            PlaneFetch::Empty,
            Some("https://plane.example"),
            "owner/repo",
            path,
        );
        let unconfigured =
            missing_baseline_hint(PlaneFetch::NotConfigured, None, "owner/repo", path);
        assert_ne!(empty, unconfigured);
        assert!(
            !empty.contains("no plane configured"),
            "the plane is configured; it is just empty: {empty}"
        );
    }
}
