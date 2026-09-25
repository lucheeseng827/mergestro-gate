// SPDX-License-Identifier: Apache-2.0
//! MCP lane: fail the PR when the repo's own MCP server breaks.
//!
//! The other lanes ask whether the *diff* is sound. This one asks whether the
//! thing the diff builds still behaves — it spawns the repository's own MCP
//! server and talks to it over stdio, sending the malformed frames, truncated
//! bodies and out-of-order requests a real client eventually sends by accident,
//! then fails the merge when the server answers one of them wrongly.
//!
//! It is a *driver*, not a prober. The probing is done by `specprobe`, invoked
//! as an external binary through [`CommandRunner`] exactly as the Rust engine
//! invokes `cargo-mutants`. That split is deliberate and it is not only about
//! licences: the catalog of checks, their severities, and the mapping from "a
//! check failed" to "this would be denied admission" all live with the prober.
//! A CI lane that carried its own copy of which check ids are Critical would
//! drift out of date the first time the catalog grew, and would do so silently
//! — in the direction of passing.
//!
//! So the contract with `specprobe` is narrow and machine-readable:
//!
//! * the lane sets `SPECPROBE_FAIL_ON` to the configured threshold;
//! * `specprobe` exits non-zero when that threshold is tripped;
//! * `specprobe` prints the evidence run, carrying a `gate` object that names
//!   the checks it is failing the build on and why.
//!
//! The exit code is the decision and the JSON is the explanation. When they
//! disagree — or when the JSON is unreadable — the lane blocks and says so.
//!
//! ## What this lane will never do
//!
//! **A check the probe skipped is not a pass and is not a failure.** Over stdio
//! roughly a third of the catalog cannot be expressed at all (the header and
//! cross-principal checks need an HTTP layer), and an era-specific check does
//! not apply to a server speaking the other era. Those come back `skip`, they
//! are reported as skips with their count, and they never gate. A gate that
//! turned "could not ask" into "failed" would block every conformant server the
//! first time it was pointed at one, and the fix a team would reach for is
//! turning the gate off.
//!
//! **A lane that could not run is not a lane that passed.** A missing
//! `specprobe`, a server that will not build, a probe that dies — each blocks
//! with the reason printed, rather than quietly contributing nothing to the
//! verdict. The gate cannot certify what it did not run.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Config, McpServerTarget};
use crate::runner::CommandRunner;

/// Env var through which `specprobe` is told how strict to be.
const FAIL_ON_VAR: &str = "SPECPROBE_FAIL_ON";

/// How much of a failing command's output is carried into the report. Long
/// enough to hold a build error's last few lines, short enough that a runaway
/// log cannot dominate a PR comment.
const OUTPUT_TAIL_BYTES: usize = 1200;

/// What the lane established for one configured server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum McpServerStatus {
    /// The probe ran to completion. `blocking` is what the configured threshold
    /// says should fail the build — empty means this server is clear.
    Probed {
        tally: McpTally,
        blocking: Vec<McpBlockingCheck>,
        /// Whether the server stayed answerable. `None` when the prober did not report it —
        /// an older binary — which is not the same as "nothing wedged".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        liveness: Option<McpLiveness>,
        /// The prober's own version string, from the evidence run.
        #[serde(skip_serializing_if = "Option::is_none")]
        suite_version: Option<String>,
    },
    /// The lane could not produce a result. This **blocks** — see the module
    /// docs — and carries why.
    Unrunnable { reason: String },
}

/// Whether the server stayed answerable while the probe ran.
///
/// This is the prior question to every check result, and for a merge gate it is usually the
/// *only* question: the failure a PR most often introduces is not "the server answered wrongly",
/// it is "the server stopped answering". Mirrors the prober's own summary — the gate reads these
/// numbers, it does not compute them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpLiveness {
    #[serde(default)]
    pub exchanges: usize,
    /// Frames that drew no reply. **Expected** on the malformed-framing checks — an unparseable
    /// frame carries no id to answer, so a conformant server drops it. Not the badness number.
    #[serde(default)]
    pub timed_out: usize,
    #[serde(default)]
    pub crashed: usize,
    /// Times the connection was *proven* to have stopped working. This is the badness number.
    #[serde(default)]
    pub wedges: usize,
    /// Times it came back on its own, with nothing re-established underneath it.
    #[serde(default)]
    pub recovered: usize,
    /// Times the connection had to be rebuilt to get past a wedge. For a client that is not a
    /// probe, this is the session ending.
    #[serde(default)]
    pub restarts: usize,
    /// The probe finished with the server still not answering.
    #[serde(default)]
    pub ended_down: bool,
    #[serde(default)]
    pub worst_latency_ms: u64,
}

impl McpLiveness {
    /// Whether this is worth putting in front of a PR author at all.
    pub fn notable(&self) -> bool {
        self.wedges > 0 || self.ended_down
    }

    /// The line that goes first, above any check id.
    pub fn headline(&self) -> String {
        if self.ended_down {
            return format!(
                "**the server stopped answering and never came back** — it went away {} time(s) \
                 across {} exchanges",
                self.wedges, self.exchanges
            );
        }
        if self.restarts > 0 {
            return format!(
                "**the server stopped answering {} time(s)** across {} exchanges and needed the \
                 connection rebuilt {} time(s) — for a client that is the session ending, not a \
                 recovery",
                self.wedges, self.exchanges, self.restarts
            );
        }
        if self.wedges > 0 {
            return format!(
                "the server stopped answering {} time(s) across {} exchanges, and came back on \
                 its own each time",
                self.wedges, self.exchanges
            );
        }
        format!(
            "stayed answerable across all {} exchanges (worst answer {}ms)",
            self.exchanges, self.worst_latency_ms
        )
    }
}

/// A probe run's coverage, mirroring the prober's own tally so the number in a
/// PR comment and the number on the admission board are the same number.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpTally {
    /// Checks that produced a pass or a fail — the run's real coverage.
    #[serde(default)]
    pub scored: usize,
    #[serde(default)]
    pub passed: usize,
    #[serde(default)]
    pub failed: usize,
    #[serde(default)]
    pub errored: usize,
    #[serde(default)]
    pub skipped: usize,
}

/// One check the prober is failing the build on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpBlockingCheck {
    pub check: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(default)]
    pub outcome: String,
    /// Why this trips the threshold, in the prober's words.
    #[serde(default)]
    pub reason: String,
    /// What the probe actually saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One configured server's outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerOutcome {
    pub name: String,
    #[serde(flatten)]
    pub status: McpServerStatus,
}

impl McpServerOutcome {
    /// Whether this outcome should fail the build. Both arms of "not clear" —
    /// a check the threshold gates on, and a lane that could not run — block.
    pub fn blocks(&self) -> bool {
        match &self.status {
            McpServerStatus::Probed { blocking, .. } => !blocking.is_empty(),
            McpServerStatus::Unrunnable { .. } => true,
        }
    }

    /// The block reason for the verdict, or `None` when this server is clear.
    pub fn block_reason(&self) -> Option<String> {
        match &self.status {
            McpServerStatus::Probed {
                blocking, liveness, ..
            } if !blocking.is_empty() => {
                let ids: Vec<&str> = blocking.iter().map(|b| b.check.as_str()).collect();
                // Lead with the hang when there was one. "failed NP-FRAME-001 and NP-FRAME-005"
                // is accurate and tells a PR author almost nothing; "stopped answering and never
                // came back" is the same fact in the words they will act on.
                let lead = match liveness {
                    Some(l) if l.ended_down => format!(
                        "MCP server `{}` stopped answering and never came back",
                        self.name
                    ),
                    Some(l) if l.wedges > 0 => format!(
                        "MCP server `{}` stopped answering {} time(s) during the probe",
                        self.name, l.wedges
                    ),
                    _ => format!("MCP server `{}` failed", self.name),
                };
                Some(format!(
                    "{lead} — {} conformance check(s): {}",
                    ids.len(),
                    ids.join(", ")
                ))
            }
            McpServerStatus::Unrunnable { reason } => Some(format!(
                "the MCP lane could not run for `{}`: {reason}",
                self.name
            )),
            _ => None,
        }
    }
}

/// The lane's contribution to a [`crate::report::GateReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpGateReport {
    /// The threshold that was applied, echoed so a reader of the artifact knows
    /// how strict the run was without going back to the config.
    pub fail_on: String,
    pub servers: Vec<McpServerOutcome>,
}

impl McpGateReport {
    /// Servers that should fail the build.
    pub fn blocking(&self) -> impl Iterator<Item = &McpServerOutcome> {
        self.servers.iter().filter(|s| s.blocks())
    }

    /// Whether every configured, in-scope server came back clear.
    pub fn all_clear(&self) -> bool {
        self.servers.iter().all(|s| !s.blocks())
    }
}

/// Run the lane for every configured server the diff puts in scope.
///
/// `changed` is every repo-relative path the diff touches — deletions included,
/// and of any extension. It deliberately is **not** derived from the unified
/// diff text: that text carries only the files a mutation engine or the docs
/// lane can use, so a server whose tool schema, lockfile or Dockerfile changed
/// would look untouched, which is precisely the change most likely to break it.
///
/// Returns `None` when the lane has nothing to say — no servers configured, or
/// none of them touched by this diff. `None` is *not* a pass with no findings;
/// it means the lane did not apply, and nothing is rendered for it.
pub fn run(runner: &dyn CommandRunner, cfg: &Config, changed: &[String]) -> Option<McpGateReport> {
    if cfg.mcp_servers.is_empty() {
        return None;
    }
    let in_scope: Vec<&McpServerTarget> = cfg
        .mcp_servers
        .iter()
        .filter(|t| applies(t, changed))
        .collect();
    if in_scope.is_empty() {
        return None;
    }
    let servers = in_scope
        .into_iter()
        .map(|t| McpServerOutcome {
            name: t.name.clone(),
            status: probe_one(runner, cfg, t),
        })
        .collect();
    Some(McpGateReport {
        fail_on: cfg.mcp_fail_on.clone(),
        servers,
    })
}

/// Does this diff touch anything this server is declared to cover?
fn applies(target: &McpServerTarget, touched: &[String]) -> bool {
    touched
        .iter()
        .any(|p| target.paths.iter().any(|scope| path_covers(scope, p)))
}

/// Prefix match on path *segments*, so `crates/mcp` does not claim
/// `crates/mcp-unrelated/src/lib.rs`. An empty scope covers the whole repo.
fn path_covers(scope: &str, path: &str) -> bool {
    let scope = scope.trim_matches('/');
    if scope.is_empty() {
        return true;
    }
    path == scope || path.starts_with(&format!("{scope}/"))
}

/// Build (if configured) and probe one server.
fn probe_one(
    runner: &dyn CommandRunner,
    cfg: &Config,
    target: &McpServerTarget,
) -> McpServerStatus {
    let work_dir = work_dir(cfg, target);

    if let Some((program, args)) = split_command(&target.build) {
        match runner.run(program, &args, &work_dir) {
            Ok(out) if out.success => {}
            Ok(out) => {
                return McpServerStatus::Unrunnable {
                    reason: format!(
                        "the build command `{}` failed with status {}: {}",
                        target.build.join(" "),
                        out.code
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "signal".into()),
                        tail(&out.combined())
                    ),
                }
            }
            Err(e) => {
                return McpServerStatus::Unrunnable {
                    reason: format!("could not run the build command `{program}`: {e}"),
                }
            }
        }
    }

    let Some((server_program, server_args)) = split_command(&target.command) else {
        // Rejected by `Config::validate`; reachable only if a caller built a
        // Config by hand, and still not a reason to report a clean run.
        return McpServerStatus::Unrunnable {
            reason: "no server command is configured".to_string(),
        };
    };
    let args_json = serde_json::to_string(&server_args).unwrap_or_else(|_| "[]".to_string());
    let timeout = target.timeout_secs.to_string();

    let mut env: Vec<(&str, &str)> = vec![
        ("SPECPROBE_SERVER", server_program),
        ("SPECPROBE_SERVER_ARGS", args_json.as_str()),
        ("SPECPROBE_TIMEOUT_SECS", timeout.as_str()),
        ("SPECPROBE_SPEC_VERSION", target.spec_version.as_str()),
        (FAIL_ON_VAR, cfg.mcp_fail_on.as_str()),
    ];
    if let Some(tool) = &target.elicit_tool {
        env.push(("SPECPROBE_ELICIT_TOOL", tool.as_str()));
    }

    let out = match runner.run_env(&cfg.specprobe_bin, &[], &env, &work_dir) {
        Ok(out) => out,
        Err(e) => {
            return McpServerStatus::Unrunnable {
                reason: format!(
                    "could not run `{}`: {e}. Install the prober or set `specprobe_bin` \
                     to its path.",
                    cfg.specprobe_bin
                ),
            }
        }
    };

    match parse_gate(&out.stdout) {
        // The exit code is the decision and the JSON is the explanation. When
        // they disagree the lane has no defensible reading of the run, so it
        // says so rather than picking the convenient one — and the convenient
        // one here is "clear", which is exactly why this arm exists.
        Some(run) if out.code != Some(0) && run.blocking.is_empty() => {
            McpServerStatus::Unrunnable {
                reason: format!(
                    "`{}` exited {} but its gate report names no blocking check — the exit code \
                     and the report disagree, so the run cannot be read either way",
                    cfg.specprobe_bin,
                    out.code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "on a signal".into()),
                ),
            }
        }
        Some(run) => McpServerStatus::Probed {
            tally: run.tally,
            blocking: run.blocking,
            liveness: run.liveness,
            suite_version: run.suite_version,
        },
        None => McpServerStatus::Unrunnable {
            reason: format!(
                "`{}` exited {} without a readable gate report — the lane cannot tell a clean \
                 run from a failed one, so it blocks: {}",
                cfg.specprobe_bin,
                out.code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "on a signal".into()),
                tail(&out.stderr)
            ),
        },
    }
}

/// The directory the build and probe run in. `dir` is validated
/// repo-relative, so this always stays inside the checkout.
fn work_dir(cfg: &Config, target: &McpServerTarget) -> PathBuf {
    match &target.dir {
        Some(d) => cfg.repo.join(d),
        None => cfg.repo.clone(),
    }
}

/// Split a `[program, args…]` vector, `None` when empty.
fn split_command(cmd: &[String]) -> Option<(&str, Vec<&str>)> {
    let (program, args) = cmd.split_first()?;
    Some((program.as_str(), args.iter().map(String::as_str).collect()))
}

/// Pull the `gate` object out of an evidence run.
///
/// `None` when the output is not an evidence run carrying a gate report — which
/// is how the caller learns the prober did not get far enough to have an
/// opinion, and blocks. Note what this does *not* do: it never treats an absent
/// `gate` key as "nothing to report". The lane always sets `SPECPROBE_FAIL_ON`,
/// so a run without a gate object is a run that did not happen as asked.
fn parse_gate(stdout: &str) -> Option<ParsedRun> {
    let run: Value = serde_json::from_str(stdout.trim()).ok()?;
    let gate = run.get("gate")?;
    let tally: McpTally = serde_json::from_value(gate.get("tally")?.clone()).ok()?;
    let blocking: Vec<McpBlockingCheck> =
        serde_json::from_value(gate.get("blocking")?.clone()).ok()?;
    // Optional, unlike the two above: a prober that reports no liveness is an older one, not a
    // broken run. Absent renders as absent rather than as a reassuring "stayed up".
    let liveness = run
        .get("liveness")
        .and_then(|l| serde_json::from_value(l.clone()).ok());
    let suite_version = run
        .get("suite_version")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(ParsedRun {
        tally,
        blocking,
        liveness,
        suite_version,
    })
}

/// What [`parse_gate`] recovered from one prober invocation.
struct ParsedRun {
    tally: McpTally,
    blocking: Vec<McpBlockingCheck>,
    liveness: Option<McpLiveness>,
    suite_version: Option<String>,
}

/// The last [`OUTPUT_TAIL_BYTES`] of a command's output, trimmed, on one line
/// per source line — enough to diagnose, bounded enough for a PR comment.
fn tail(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return "(no output)".to_string();
    }
    // The first char boundary from the left whose tail fits the budget. Indexing
    // by byte offset alone would slice through a multi-byte character and panic,
    // and a build log is exactly where a stray non-ASCII byte turns up.
    let start = text
        .char_indices()
        .map(|(i, _)| i)
        .find(|i| text.len() - i <= OUTPUT_TAIL_BYTES)
        .unwrap_or(0);
    let cut = &text[start..];
    if start == 0 {
        cut.to_string()
    } else {
        format!("…{cut}")
    }
}

/// A one-line summary of one server's outcome, shared by the text and Markdown
/// renderers so they cannot describe the same run differently.
pub fn summary_line(outcome: &McpServerOutcome) -> String {
    match &outcome.status {
        McpServerStatus::Probed {
            tally, blocking, ..
        } => format!(
            "{}: {} passed · {} failed · {} errored · {} skipped ({} scored){}",
            outcome.name,
            tally.passed,
            tally.failed,
            tally.errored,
            tally.skipped,
            tally.scored,
            if blocking.is_empty() {
                String::new()
            } else {
                format!(" — {} blocking", blocking.len())
            }
        ),
        McpServerStatus::Unrunnable { reason } => {
            format!("{}: did not run — {reason}", outcome.name)
        }
    }
}

/// Where a lane's working directory would land, for `Config::validate`'s sake:
/// a repo-relative path with no `..` and no root. A PR can edit the gate's own
/// config file, so the lane must not be steerable out of the checkout.
pub(crate) fn is_safe_relative(dir: &str) -> bool {
    let p = Path::new(dir);
    !p.is_absolute()
        && !dir.is_empty()
        && p.components().all(|c| {
            matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerTarget;
    use crate::runner::test_support::ScriptedRunner;

    fn target(name: &str, paths: &[&str]) -> McpServerTarget {
        McpServerTarget {
            name: name.to_string(),
            paths: paths.iter().map(|s| s.to_string()).collect(),
            dir: None,
            build: Vec::new(),
            command: vec!["node".into(), "server.js".into()],
            spec_version: "2025-11-25".into(),
            timeout_secs: 20,
            elicit_tool: None,
        }
    }

    fn cfg_with(targets: Vec<McpServerTarget>) -> Config {
        Config {
            mcp_servers: targets,
            ..Config::default()
        }
    }

    fn touching(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_string()).collect()
    }

    /// An evidence run as `specprobe` prints it in gate mode, carrying a liveness summary at the
    /// TOP level of the run — the same placement `specprobe` uses (`"liveness"` beside
    /// `"coverage"`, not inside `"gate"`). That placement is a cross-file contract with the
    /// prober: if either side moves the key, `parse_gate` reads `None`, the gate calls it an
    /// older binary, and the "stopped answering" headline silently disappears.
    fn evidence_with_liveness(
        passed: usize,
        failed: usize,
        skipped: usize,
        blocking: &[&str],
        liveness: Option<Value>,
    ) -> String {
        let mut run: Value = serde_json::from_str(&evidence(passed, failed, skipped, blocking))
            .expect("the fixture is valid JSON");
        if let Some(l) = liveness {
            run["liveness"] = l;
        }
        run.to_string()
    }

    /// An evidence run as `specprobe` prints it in gate mode.
    fn evidence(passed: usize, failed: usize, skipped: usize, blocking: &[&str]) -> String {
        let blocking: Vec<Value> = blocking
            .iter()
            .map(|id| {
                serde_json::json!({
                    "check": id,
                    "severity": "critical",
                    "outcome": "fail",
                    "reason": "failed a Critical check",
                    "detail": "served a request it should have refused",
                })
            })
            .collect();
        serde_json::json!({
            "source": "specprobe_negative",
            "spec_version": "2025-11-25",
            "suite_version": "specprobe/0.1.0",
            "results": [],
            "gate": {
                "fail_on": "critical",
                "blocking": blocking,
                "tally": {
                    "scored": passed + failed,
                    "passed": passed,
                    "failed": failed,
                    "errored": 0,
                    "skipped": skipped,
                    "skipped_checks": [],
                },
            },
        })
        .to_string()
    }

    #[test]
    fn no_configured_servers_means_the_lane_does_not_apply() {
        let runner = ScriptedRunner::new();
        assert!(run(&runner, &Config::default(), &touching(&["src/lib.rs"])).is_none());
        assert!(
            runner.calls().is_empty(),
            "nothing should have been spawned"
        );
    }

    #[test]
    fn a_diff_that_misses_every_server_does_not_spawn_a_probe() {
        // The cost control: probing is expensive, so a PR touching the docs must
        // not pay for it.
        let cfg = cfg_with(vec![target("acme", &["servers/acme"])]);
        let runner = ScriptedRunner::new();
        assert!(run(&runner, &cfg, &touching(&["README.md"])).is_none());
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn scope_matching_respects_path_segments() {
        assert!(path_covers("servers/acme", "servers/acme/src/main.rs"));
        assert!(path_covers("servers/acme", "servers/acme"));
        // The bug this guards: a prefix match on raw strings claims a sibling.
        assert!(!path_covers(
            "servers/acme",
            "servers/acme-unrelated/src/main.rs"
        ));
        assert!(!path_covers("servers/acme", "servers/acmex"));
        // Leading/trailing slashes in config are tolerated.
        assert!(path_covers("/servers/acme/", "servers/acme/src/main.rs"));
    }

    #[test]
    fn a_non_source_file_puts_its_server_in_scope() {
        // The scoping input is every changed path, not the unified diff text —
        // that text carries only files a mutation engine can use, so a changed
        // tool schema would look like an untouched server.
        let cfg = cfg_with(vec![target("acme", &["servers/acme"])]);
        let runner = ScriptedRunner::new();
        runner.push_ok(&evidence(17, 0, 17, &[]));
        let report =
            run(&runner, &cfg, &touching(&["servers/acme/tools.json"])).expect("lane applies");
        assert!(report.all_clear());
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn a_clean_probe_passes_and_carries_the_coverage_it_actually_had() {
        let cfg = cfg_with(vec![target("acme", &["servers/acme"])]);
        let runner = ScriptedRunner::new();
        runner.push_ok(&evidence(17, 0, 17, &[]));
        let report = run(&runner, &cfg, &touching(&["servers/acme/src/main.rs"])).unwrap();
        assert!(report.all_clear());
        let McpServerStatus::Probed {
            tally,
            suite_version,
            ..
        } = &report.servers[0].status
        else {
            panic!("expected a probed status: {:?}", report.servers[0]);
        };
        // The honest headline: 17 checks skipped, and the report says so rather
        // than presenting a 17-skip run as a fully-exercised one.
        assert_eq!((tally.passed, tally.failed, tally.skipped), (17, 0, 17));
        assert_eq!(suite_version.as_deref(), Some("specprobe/0.1.0"));
    }

    #[test]
    fn a_gating_failure_blocks_and_names_the_check() {
        let cfg = cfg_with(vec![target("acme", &["servers/acme"])]);
        let runner = ScriptedRunner::new();
        // A gating run exits 1 and still prints the evidence — the exit code is
        // the decision, the JSON is the explanation.
        runner.push(
            1,
            &evidence(16, 1, 17, &["NP-FRAME-001"]),
            "specprobe: gate fail_on=critical — 1 gating result(s): NP-FRAME-001",
        );
        let report = run(&runner, &cfg, &touching(&["servers/acme/src/main.rs"])).unwrap();
        assert!(!report.all_clear());
        let reason = report.servers[0].block_reason().unwrap();
        assert!(reason.contains("NP-FRAME-001"), "{reason}");
        assert!(reason.contains("acme"), "{reason}");
    }

    #[test]
    fn skips_alone_never_block() {
        // The lane's load-bearing property. Over stdio a third of the catalog
        // cannot be expressed; if that read as a failure the gate would block
        // every conformant server and get switched off.
        let cfg = cfg_with(vec![target("acme", &["servers/acme"])]);
        let runner = ScriptedRunner::new();
        runner.push_ok(&evidence(0, 0, 34, &[]));
        let report = run(&runner, &cfg, &touching(&["servers/acme/src/main.rs"])).unwrap();
        assert!(report.all_clear(), "a fully-skipped run must not block");
    }

    #[test]
    fn a_missing_prober_blocks_rather_than_passing_silently() {
        let cfg = cfg_with(vec![target("acme", &["servers/acme"])]);
        let runner = ScriptedRunner::new();
        runner.push_err("No such file or directory (os error 2)");
        let report = run(&runner, &cfg, &touching(&["servers/acme/src/main.rs"])).unwrap();
        assert!(!report.all_clear());
        let reason = report.servers[0].block_reason().unwrap();
        assert!(reason.contains("could not run"), "{reason}");
        assert!(reason.contains("specprobe_bin"), "{reason}");
    }

    #[test]
    fn unreadable_output_blocks_even_when_the_prober_exited_zero() {
        // The dangerous shape: a prober that ran but printed something the lane
        // cannot read. Trusting the exit code alone would call that a pass.
        let cfg = cfg_with(vec![target("acme", &["servers/acme"])]);
        let runner = ScriptedRunner::new();
        runner.push(0, "not json at all", "");
        let report = run(&runner, &cfg, &touching(&["servers/acme/src/main.rs"])).unwrap();
        assert!(!report.all_clear());
        assert!(report.servers[0]
            .block_reason()
            .unwrap()
            .contains("without a readable gate report"));
    }

    #[test]
    fn liveness_is_read_from_the_top_level_of_the_run() {
        // The one branch that turns the prober's liveness into a probed status was previously
        // untested — a moved key would have silently downgraded every comment to "older binary".
        let cfg = cfg_with(vec![target("acme", &["servers/acme"])]);
        let runner = ScriptedRunner::new();
        runner.push(
            1,
            &evidence_with_liveness(
                16,
                1,
                17,
                &["NP-FRAME-001"],
                Some(serde_json::json!({
                    "exchanges": 34, "timed_out": 12, "crashed": 0,
                    "wedges": 3, "recovered": 0, "restarts": 2,
                    "ended_down": true, "worst_latency_ms": 119,
                })),
            ),
            "",
        );
        let report = run(&runner, &cfg, &touching(&["servers/acme/src/main.rs"])).unwrap();
        let McpServerStatus::Probed {
            liveness: Some(l), ..
        } = &report.servers[0].status
        else {
            panic!("liveness must survive the parse: {:?}", report.servers[0]);
        };
        assert!(l.ended_down);
        // ...and the block reason leads with the hang, not the check id.
        assert!(report.servers[0]
            .block_reason()
            .unwrap()
            .starts_with("MCP server `acme` stopped answering and never came back"));
    }

    #[test]
    fn every_spawn_stays_inside_the_checkout() {
        // The containment claim `Config::validate` makes (`dir` is repo-relative, no `..`)
        // proven at the spawn site: both the build and the probe run under the repo.
        let mut t = target("acme", &["servers/acme"]);
        t.build = vec!["npm".into(), "run".into(), "build".into()];
        t.dir = Some("servers/acme".into());
        let cfg = Config {
            repo: std::path::PathBuf::from("/checkout"),
            ..cfg_with(vec![t])
        };
        let runner = ScriptedRunner::new();
        runner.push_ok("built");
        runner.push_ok(&evidence(17, 0, 17, &[]));
        run(&runner, &cfg, &touching(&["servers/acme/src/main.rs"])).unwrap();
        for call in runner.calls() {
            assert_eq!(
                call.cwd,
                Path::new("/checkout/servers/acme"),
                "{} ran outside the declared working directory",
                call.program
            );
        }
    }

    #[test]
    fn a_nonzero_exit_with_an_empty_gate_report_blocks() {
        // The two channels disagree: the exit code says the threshold tripped,
        // the report names nothing. Trusting the report would turn a real gate
        // failure into a pass, which is the one direction that must not happen.
        let cfg = cfg_with(vec![target("acme", &["servers/acme"])]);
        let runner = ScriptedRunner::new();
        runner.push(1, &evidence(17, 0, 17, &[]), "");
        let report = run(&runner, &cfg, &touching(&["servers/acme/src/main.rs"])).unwrap();
        assert!(!report.all_clear());
        assert!(report.servers[0]
            .block_reason()
            .unwrap()
            .contains("disagree"));
    }

    #[test]
    fn an_evidence_run_with_no_gate_object_blocks() {
        // The lane always asks for a gate report. Getting a plain evidence run
        // back means the prober did not do what was asked — most likely an old
        // binary that ignores SPECPROBE_FAIL_ON. Reading it as "no blocking
        // checks" would silently disable the gate for everyone on that version.
        let cfg = cfg_with(vec![target("acme", &["servers/acme"])]);
        let runner = ScriptedRunner::new();
        runner.push(
            0,
            r#"{"source":"specprobe_negative","spec_version":"2025-11-25","results":[]}"#,
            "",
        );
        let report = run(&runner, &cfg, &touching(&["servers/acme/src/main.rs"])).unwrap();
        assert!(!report.all_clear());
    }

    #[test]
    fn a_failing_build_blocks_and_the_probe_never_runs() {
        let mut t = target("acme", &["servers/acme"]);
        t.build = vec!["npm".into(), "run".into(), "build".into()];
        let cfg = cfg_with(vec![t]);
        let runner = ScriptedRunner::new();
        runner.push_fail(2, "TS2304: Cannot find name 'foo'.");
        let report = run(&runner, &cfg, &touching(&["servers/acme/src/main.rs"])).unwrap();
        assert!(!report.all_clear());
        let reason = report.servers[0].block_reason().unwrap();
        assert!(reason.contains("build command"), "{reason}");
        assert!(reason.contains("TS2304"), "{reason}");
        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "the probe must not have been spawned");
        assert_eq!(calls[0].program, "npm");
        assert_eq!(calls[0].args, ["run", "build"]);
    }

    #[test]
    fn the_probe_is_configured_entirely_through_the_environment() {
        // If the environment were dropped, specprobe would fail on a missing
        // SPECPROBE_SERVER rather than probing the wrong thing — but this pins
        // the mapping so a rename cannot silently change which server is probed.
        let mut t = target("acme", &["servers/acme"]);
        t.command = vec!["node".into(), "dist/server.js".into(), "--stdio".into()];
        t.spec_version = "2026-07-28".into();
        t.timeout_secs = 45;
        t.elicit_tool = Some("ask_user".into());
        let mut cfg = cfg_with(vec![t]);
        cfg.mcp_fail_on = "unproven".into();
        let runner = ScriptedRunner::new();
        runner.push_ok(&evidence(17, 0, 17, &[]));
        run(&runner, &cfg, &touching(&["servers/acme/src/main.rs"])).unwrap();

        let call = &runner.calls()[0];
        assert_eq!(call.program, "specprobe");
        assert_eq!(call.env_var("SPECPROBE_SERVER"), Some("node"));
        // Args go over as JSON, never a shell string — a path with a space in it
        // must not become two arguments.
        assert_eq!(
            call.env_var("SPECPROBE_SERVER_ARGS"),
            Some(r#"["dist/server.js","--stdio"]"#)
        );
        assert_eq!(call.env_var("SPECPROBE_SPEC_VERSION"), Some("2026-07-28"));
        assert_eq!(call.env_var("SPECPROBE_TIMEOUT_SECS"), Some("45"));
        assert_eq!(call.env_var("SPECPROBE_ELICIT_TOOL"), Some("ask_user"));
        assert_eq!(call.env_var(FAIL_ON_VAR), Some("unproven"));
    }

    #[test]
    fn never_still_reports_but_stops_blocking() {
        // `mcp_fail_on: never` is the advisory posture: the prober reports what
        // it found and nothing gates. It reaches the prober as the threshold, so
        // `blocking` comes back empty and the lane has nothing to block on.
        let cfg = Config {
            mcp_fail_on: "never".into(),
            ..cfg_with(vec![target("acme", &["servers/acme"])])
        };
        let runner = ScriptedRunner::new();
        runner.push_ok(&evidence(16, 1, 17, &[]));
        let report = run(&runner, &cfg, &touching(&["servers/acme/src/main.rs"])).unwrap();
        assert!(report.all_clear());
        assert_eq!(runner.calls()[0].env_var(FAIL_ON_VAR), Some("never"));
    }

    #[test]
    fn every_in_scope_server_is_probed_independently() {
        let cfg = cfg_with(vec![
            target("acme", &["servers/acme"]),
            target("beta", &["servers/beta"]),
            target("gamma", &["servers/gamma"]),
        ]);
        let runner = ScriptedRunner::new();
        runner.push_ok(&evidence(17, 0, 17, &[]));
        runner.push(1, &evidence(16, 1, 17, &["NP-LIFE-001"]), "");
        let report = run(
            &runner,
            &cfg,
            &touching(&["servers/acme/a.rs", "servers/beta/b.rs"]),
        )
        .unwrap();
        assert_eq!(report.servers.len(), 2, "gamma was not touched");
        assert_eq!(report.blocking().count(), 1);
        assert_eq!(report.blocking().next().unwrap().name, "beta");
    }

    #[test]
    fn the_tail_is_bounded_and_marks_what_it_dropped() {
        let long = "x".repeat(OUTPUT_TAIL_BYTES * 2);
        let t = tail(&long);
        assert!(t.starts_with('…'));
        assert!(t.len() <= OUTPUT_TAIL_BYTES + '…'.len_utf8());
        assert_eq!(tail("  short  "), "short");
        assert_eq!(tail("   "), "(no output)");
    }

    #[test]
    fn the_tail_cuts_on_a_character_boundary() {
        // A build log carrying non-ASCII is ordinary (a compiler's smart quotes,
        // a path with an accent). Cutting by byte offset would slice through one
        // and panic — inside the lane whose whole job is to report the failure.
        let long = "é".repeat(OUTPUT_TAIL_BYTES); // 2 bytes each: 2× over budget
        let t = tail(&long);
        assert!(t.starts_with('…'));
        assert!(t.len() <= OUTPUT_TAIL_BYTES + '…'.len_utf8());
        assert!(t.chars().skip(1).all(|c| c == 'é'));
    }

    #[test]
    fn only_repo_relative_working_directories_are_accepted() {
        // The gate's config file lives in the repo, so a PR can edit it. The
        // lane must not become a way to run commands outside the checkout.
        assert!(is_safe_relative("servers/acme"));
        assert!(is_safe_relative("./servers/acme"));
        assert!(!is_safe_relative("/etc"));
        assert!(!is_safe_relative("../../elsewhere"));
        assert!(!is_safe_relative("servers/../../elsewhere"));
        assert!(!is_safe_relative(""));
    }

    #[test]
    fn summary_lines_state_both_outcomes_without_hedging() {
        let clear = McpServerOutcome {
            name: "acme".into(),
            status: McpServerStatus::Probed {
                tally: McpTally {
                    scored: 17,
                    passed: 17,
                    failed: 0,
                    errored: 0,
                    skipped: 17,
                },
                blocking: Vec::new(),
                liveness: None,
                suite_version: None,
            },
        };
        let line = summary_line(&clear);
        assert!(line.contains("17 skipped"), "{line}");
        assert!(!line.contains("blocking"), "{line}");

        let broken = McpServerOutcome {
            name: "beta".into(),
            status: McpServerStatus::Unrunnable {
                reason: "no prober".into(),
            },
        };
        assert!(summary_line(&broken).contains("did not run"));
    }
}
