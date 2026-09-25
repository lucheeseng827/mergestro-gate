// SPDX-License-Identifier: Apache-2.0
//! Result types and rendering for the gate run.

use serde::{Deserialize, Serialize};

use crate::debt::DebtDelta;
use crate::mcp_gate::{self, McpGateReport, McpServerStatus};
use crate::pattern::PatternReport;
use crate::severity::{self, Severity};
use crate::turnover_lane::TurnoverLane;

/// A single mutation, identified by where `cargo-mutants` introduced it.
///
/// The canonical identity is the mutant *name* string emitted by
/// `cargo-mutants`, of the form `file:line:col: <description>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mutant {
    pub file: String,
    pub line: u32,
    pub column: u32,
    /// Enclosing function, when known (from `--list --json`). Survivor lines
    /// parsed from text output won't carry this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// The mutation itself, e.g. `replace > with >=`.
    pub description: String,
    /// Full original name string from `cargo-mutants`.
    pub name: String,
}

impl Mutant {
    /// Parse a `cargo-mutants` name line: `src/foo.rs:12:5: replace > with >=`.
    ///
    /// Returns `None` if the prefix isn't `path:line:col:`.
    pub fn parse_name(name: &str) -> Option<Mutant> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        // Description starts after the first ": " (colon-space). The location
        // prefix `file:line:col` contains colons but no colon-space.
        let (loc, description) = match name.split_once(": ") {
            Some((loc, rest)) => (loc, rest.to_string()),
            None => (name, String::new()),
        };
        // Peel column then line off the right; whatever remains is the path.
        let (rest, col) = loc.rsplit_once(':')?;
        let (file, line) = rest.rsplit_once(':')?;
        let column = col.trim().parse().ok()?;
        let line = line.trim().parse().ok()?;
        if file.is_empty() {
            return None;
        }
        Some(Mutant {
            file: file.to_string(),
            line,
            column,
            function: None,
            description,
            name: name.to_string(),
        })
    }

    /// Grouping key for the per-function cap. Falls back to the file when the
    /// enclosing function is unknown so caps still bound work per location.
    pub fn group_key(&self) -> String {
        match &self.function {
            Some(f) => format!("{}::{}", self.file, f),
            None => format!("{}::<file>", self.file),
        }
    }
}

/// Outcome of the determinism pre-flight.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PreflightOutcome {
    /// Pre-flight was skipped by configuration.
    Skipped,
    /// Suite was green on every run.
    Passed { runs: u32 },
    /// Suite failed outright (red before any mutation).
    Failed { run: u32, detail: String },
    /// Suite flipped between runs — flaky, results untrustworthy.
    Unstable { detail: String },
}

impl PreflightOutcome {
    /// Whether mutation testing should proceed.
    pub fn is_green(&self) -> bool {
        matches!(
            self,
            PreflightOutcome::Passed { .. } | PreflightOutcome::Skipped
        )
    }
}

/// A test that exercises code but asserts nothing — the free second signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZeroAssertionFinding {
    pub file: String,
    pub line: u32,
    pub function: String,
}

/// The gate's decision for this run (Phase 2: the gate can block).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Verdict {
    /// Nothing tripped a gate — the build may proceed.
    Pass,
    /// One or more gates tripped; each string is a human-readable reason.
    Block { reasons: Vec<String> },
}

impl Verdict {
    /// Whether this verdict should fail the build.
    pub fn is_block(&self) -> bool {
        matches!(self, Verdict::Block { .. })
    }
}

/// Hidden marker so PR comments can be located and updated in place across
/// re-runs (idempotency / merge-queue re-trigger).
pub const COMMENT_MARKER: &str = "<!-- slop-gate:report -->";

/// The full report for one gate run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateReport {
    pub base_ref: String,
    pub head_ref: String,
    pub changed_rust_files: Vec<String>,
    /// Changed `*.py` files (Python adapter). Empty unless Python was scanned.
    #[serde(default)]
    pub changed_python_files: Vec<String>,
    /// Changed JS/TS files (Stryker adapter). Empty unless JS/TS was scanned.
    #[serde(default)]
    pub changed_js_files: Vec<String>,
    /// Changed Go files (gremlins adapter). Empty unless Go was scanned.
    #[serde(default)]
    pub changed_go_files: Vec<String>,
    /// Changed Java/Kotlin files (PIT adapter). Empty unless JVM was scanned.
    #[serde(default)]
    pub changed_jvm_files: Vec<String>,
    pub preflight: PreflightOutcome,
    /// Candidate mutants on the changed surface before the cap.
    pub candidates: usize,
    /// Mutants dropped by the per-function cap (not tested).
    pub capped_out: usize,
    /// Mutants actually tested (candidates - capped_out, roughly).
    pub tested: usize,
    pub caught: usize,
    /// Survivors — mutations the suite passed over. The signal.
    pub survivors: Vec<Mutant>,
    pub timed_out: usize,
    pub unviable: usize,
    /// Tests on the changed surface that assert nothing.
    pub zero_assertion_tests: Vec<ZeroAssertionFinding>,
    /// Phase 4: net structural debt the diff adds (None when nothing changed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debt: Option<DebtDelta>,
    /// Pattern lane: AI-slop signatures on the changed surface (advisory).
    /// `None` when nothing was flagged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slop: Option<PatternReport>,
    /// Pattern lane: security anti-patterns on the changed surface (advisory).
    /// `None` when nothing was flagged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<PatternReport>,
    /// Pattern lane: convention/hallucinated-import findings (advisory).
    /// `None` when nothing was flagged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub convention: Option<PatternReport>,
    /// Pattern lane: documentation-standard compliance + doc drift on the
    /// changed modules (advisory). `None` when nothing was flagged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs: Option<PatternReport>,
    /// MCP lane: what probing this repo's own MCP server(s) established.
    /// `None` when the lane did not apply — no server declared, or none of the
    /// declared ones touched by this diff. It is never `None` to mean "ran and
    /// found nothing"; a clean run is a `Some` with no blocking checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpGateReport>,
    /// Turnover lane: the change's maintainability ratios against the
    /// repository's own baseline (`None` when the lane is disabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turnover: Option<TurnoverLane>,
    /// The gate decision.
    pub verdict: Verdict,
    pub duration_secs: f64,
}

impl GateReport {
    /// An empty report for the given range, used as a base before stages run.
    pub fn new(base_ref: &str, head_ref: &str) -> Self {
        GateReport {
            base_ref: base_ref.to_string(),
            head_ref: head_ref.to_string(),
            changed_rust_files: Vec::new(),
            changed_python_files: Vec::new(),
            changed_js_files: Vec::new(),
            changed_go_files: Vec::new(),
            changed_jvm_files: Vec::new(),
            preflight: PreflightOutcome::Skipped,
            candidates: 0,
            capped_out: 0,
            tested: 0,
            caught: 0,
            survivors: Vec::new(),
            timed_out: 0,
            unviable: 0,
            zero_assertion_tests: Vec::new(),
            debt: None,
            slop: None,
            security: None,
            convention: None,
            docs: None,
            mcp: None,
            turnover: None,
            verdict: Verdict::Pass,
            duration_secs: 0.0,
        }
    }

    /// Survivors ordered most-dangerous-first, each paired with its severity.
    pub fn ranked_survivors(&self) -> Vec<(Severity, &Mutant)> {
        severity::rank(&self.survivors)
    }

    /// Render the human-readable report for the job log.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str("── Mergestro Gate · behavioral merge gate ──\n");
        out.push_str(&format!(
            "diff:       {}...{}\n",
            self.base_ref, self.head_ref
        ));

        if self.changed_rust_files.is_empty()
            && self.changed_python_files.is_empty()
            && self.changed_js_files.is_empty()
            && self.changed_go_files.is_empty()
            && self.changed_jvm_files.is_empty()
        {
            out.push_str(
                "changed:    no Rust, Python, JS/TS, Go or Java/Kotlin files changed — nothing to mutate.\n",
            );
            // The turnover lane reads history, not the diff, so it can have a
            // verdict even when nothing was mutated.
            if let Some(t) = &self.turnover {
                out.push_str(&format!("turnover:   {}\n", turnover_line(t)));
                for line in t.text.lines() {
                    out.push_str(&format!("            {line}\n"));
                }
            }
            out.push_str(&format!("verdict:    {}\n", verdict_line(&self.verdict)));
            return out;
        }
        out.push_str(&format!(
            "changed:    {} Rust + {} Python + {} JS/TS + {} Go + {} Java/Kotlin file(s)\n",
            self.changed_rust_files.len(),
            self.changed_python_files.len(),
            self.changed_js_files.len(),
            self.changed_go_files.len(),
            self.changed_jvm_files.len()
        ));
        out.push_str(&format!(
            "preflight:  {}\n",
            preflight_line(&self.preflight)
        ));

        if self.preflight.is_green() {
            out.push_str(&format!(
                "mutants:    {} candidate(s), {} tested, {} capped out\n",
                self.candidates, self.tested, self.capped_out
            ));
            out.push_str(&format!(
                "outcomes:   {} caught · {} timed out · {} unviable · {} SURVIVED\n",
                self.caught,
                self.timed_out,
                self.unviable,
                self.survivors.len()
            ));
        } else {
            out.push_str("            suite not green & stable — mutation suppressed.\n");
        }

        if self.check_ran_zero_assertion() {
            out.push_str(&format!(
                "no-assert:  {} test(s) with no assertions\n",
                self.zero_assertion_tests.len()
            ));
        }
        if let Some(debt) = &self.debt {
            out.push_str(&format!("debt:       {}\n", debt_line(debt)));
        }
        if let Some(t) = &self.turnover {
            out.push_str(&format!("turnover:   {}\n", turnover_line(t)));
            if !t.text.is_empty() {
                for line in t.text.lines() {
                    out.push_str(&format!("            {line}\n"));
                }
            }
        }
        if let Some(slop) = &self.slop {
            out.push_str(&format!(
                "slop:       score {}/100 ({} signature(s))\n",
                slop.score,
                slop.findings.len()
            ));
        }
        if let Some(sec) = &self.security {
            out.push_str(&format!(
                "security:   score {}/100 ({} anti-pattern(s))\n",
                sec.score,
                sec.findings.len()
            ));
        }
        if let Some(conv) = &self.convention {
            out.push_str(&format!(
                "convention: score {}/100 ({} unknown import(s))\n",
                conv.score,
                conv.findings.len()
            ));
        }
        if let Some(docs) = &self.docs {
            out.push_str(&format!(
                "docs:       score {}/100 ({} finding(s))\n",
                docs.score,
                docs.findings.len()
            ));
        }
        if let Some(mcp) = &self.mcp {
            let blocking: usize = mcp.blocking().count();
            // "Probed" counts only servers that actually got probed. A run where
            // the prober was missing must not read "1 server(s) probed" — the
            // lane's own doctrine is that a lane which could not run is not a
            // lane that ran.
            let probed = mcp
                .servers
                .iter()
                .filter(|s| matches!(s.status, McpServerStatus::Probed { .. }))
                .count();
            out.push_str(&format!(
                "mcp:        {} of {} server(s) probed, {} blocking (fail_on: {})\n",
                probed,
                mcp.servers.len(),
                blocking,
                mcp.fail_on
            ));
        }
        out.push_str(&format!("runtime:    {:.1}s\n", self.duration_secs));
        out.push_str(&format!("verdict:    {}\n", verdict_line(&self.verdict)));

        if !self.survivors.is_empty() {
            out.push_str(
                "\nSurvivors — the suite passed over these mutations (most severe first):\n",
            );
            for (sev, m) in self.ranked_survivors() {
                out.push_str(&format!(
                    "  • [{}] {}:{}:{}  {}\n",
                    sev.label(),
                    m.file,
                    m.line,
                    m.column,
                    m.description
                ));
            }
        }
        if !self.zero_assertion_tests.is_empty() {
            out.push_str("\nTests with no assertions:\n");
            for z in &self.zero_assertion_tests {
                out.push_str(&format!("  • {}:{}  {}\n", z.file, z.line, z.function));
            }
        }
        if let Some(slop) = &self.slop {
            if !slop.findings.is_empty() {
                out.push_str("\nSlop signatures (advisory):\n");
                for f in &slop.findings {
                    out.push_str(&format!(
                        "  • [{}] {}:{}  {}\n",
                        f.rule, f.file, f.line, f.message
                    ));
                }
            }
        }
        if let Some(sec) = &self.security {
            if !sec.findings.is_empty() {
                out.push_str("\nSecurity anti-patterns (advisory):\n");
                for f in &sec.findings {
                    out.push_str(&format!(
                        "  • [{}] {}:{}  {}\n",
                        f.rule, f.file, f.line, f.message
                    ));
                }
            }
        }
        if let Some(conv) = &self.convention {
            if !conv.findings.is_empty() {
                out.push_str("\nConvention / hallucinated imports (advisory):\n");
                for f in &conv.findings {
                    out.push_str(&format!(
                        "  • [{}] {}:{}  {}\n",
                        f.rule, f.file, f.line, f.message
                    ));
                }
            }
        }
        if let Some(docs) = &self.docs {
            if !docs.findings.is_empty() {
                out.push_str("\nDocumentation standard (advisory):\n");
                for f in &docs.findings {
                    out.push_str(&format!("  • [{}] {}  {}\n", f.rule, f.file, f.message));
                }
            }
        }
        if let Some(mcp) = &self.mcp {
            out.push_str("\nMCP conformance — this repo's own server(s), probed over stdio:\n");
            for server in &mcp.servers {
                out.push_str(&format!("  • {}\n", mcp_gate::summary_line(server)));
                if let McpServerStatus::Probed {
                    liveness: Some(l), ..
                } = &server.status
                {
                    out.push_str(&format!(
                        "      liveness: {}\n",
                        l.headline().replace("**", "")
                    ));
                }
                if let McpServerStatus::Probed { blocking, .. } = &server.status {
                    for b in blocking {
                        out.push_str(&format!(
                            "      ✗ [{}] {} — {}\n",
                            b.severity.as_deref().unwrap_or("unclassified"),
                            b.check,
                            b.detail.as_deref().unwrap_or(b.reason.as_str())
                        ));
                    }
                }
            }
        }
        out
    }

    /// Render the report as a Markdown PR comment, carrying [`COMMENT_MARKER`]
    /// so re-runs update the same comment instead of stacking new ones.
    pub fn render_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(COMMENT_MARKER);
        out.push('\n');
        let badge = match &self.verdict {
            Verdict::Pass => "✅ **Mergestro Gate: passed**",
            Verdict::Block { .. } => "❌ **Mergestro Gate: blocked**",
        };
        out.push_str(&format!("## {badge}\n\n"));
        out.push_str(&format!(
            "`{}...{}` · {} Rust + {} Python + {} JS/TS + {} Go + {} Java/Kotlin file(s) changed · {:.1}s\n\n",
            self.base_ref,
            self.head_ref,
            self.changed_rust_files.len(),
            self.changed_python_files.len(),
            self.changed_js_files.len(),
            self.changed_go_files.len(),
            self.changed_jvm_files.len(),
            self.duration_secs
        ));

        if let Verdict::Block { reasons } = &self.verdict {
            out.push_str("**Why this is blocked:**\n");
            for r in reasons {
                out.push_str(&format!("- {r}\n"));
            }
            out.push('\n');
        }

        if !self.survivors.is_empty() {
            out.push_str("### Surviving mutations\n\n");
            out.push_str(
                "Your tests pass even with these changes applied (most severe first):\n\n",
            );
            out.push_str("| Severity | Location | Mutation |\n| --- | --- | --- |\n");
            for (sev, m) in self.ranked_survivors() {
                out.push_str(&format!(
                    "| {} | `{}:{}:{}` | {} |\n",
                    severity_badge(sev),
                    m.file,
                    m.line,
                    m.column,
                    m.description
                ));
            }
            out.push('\n');
        }

        if let Some(debt) = &self.debt {
            if debt.score() != 0 {
                out.push_str("### Debt-delta\n\n");
                out.push_str(&format!("{}\n\n", debt_line(debt)));
            }
        }

        if let Some(t) = &self.turnover {
            match t.status.as_str() {
                "skipped" => {
                    out.push_str("### Maintainability drift (turnover)\n\n");
                    out.push_str(&format!(
                        "_Not measured_: {}\n\n",
                        t.reason.as_deref().unwrap_or("skipped")
                    ));
                }
                _ => {
                    out.push_str(&format!(
                        "### Maintainability drift (turnover{})\n\n",
                        if t.mode == "advisory" {
                            ", advisory"
                        } else {
                            ""
                        }
                    ));
                    out.push_str(&t.markdown);
                    out.push('\n');
                }
            }
        }

        if !self.zero_assertion_tests.is_empty() {
            out.push_str("### Tests with no assertions\n\n");
            out.push_str("| Location | Test |\n| --- | --- |\n");
            for z in &self.zero_assertion_tests {
                out.push_str(&format!("| `{}:{}` | `{}` |\n", z.file, z.line, z.function));
            }
            out.push('\n');
        }

        if let Some(slop) = &self.slop {
            if !slop.findings.is_empty() {
                out.push_str(&format!(
                    "### Slop signatures (advisory) — score {}/100\n\n",
                    slop.score
                ));
                out.push_str("| Rule | Location | Detail |\n| --- | --- | --- |\n");
                for f in &slop.findings {
                    out.push_str(&format!(
                        "| `{}` | `{}:{}` | {} |\n",
                        f.rule, f.file, f.line, f.message
                    ));
                }
                out.push('\n');
            }
        }

        if let Some(sec) = &self.security {
            if !sec.findings.is_empty() {
                out.push_str(&format!(
                    "### Security anti-patterns (advisory) — score {}/100\n\n",
                    sec.score
                ));
                out.push_str("| Rule | Location | Detail |\n| --- | --- | --- |\n");
                for f in &sec.findings {
                    out.push_str(&format!(
                        "| `{}` | `{}:{}` | {} |\n",
                        f.rule, f.file, f.line, f.message
                    ));
                }
                out.push('\n');
            }
        }

        if let Some(conv) = &self.convention {
            if !conv.findings.is_empty() {
                out.push_str(&format!(
                    "### Convention / hallucinated imports (advisory) — score {}/100\n\n",
                    conv.score
                ));
                out.push_str("| Rule | Location | Detail |\n| --- | --- | --- |\n");
                for f in &conv.findings {
                    out.push_str(&format!(
                        "| `{}` | `{}:{}` | {} |\n",
                        f.rule, f.file, f.line, f.message
                    ));
                }
                out.push('\n');
            }
        }

        if let Some(docs) = &self.docs {
            if !docs.findings.is_empty() {
                out.push_str(&format!(
                    "### Documentation standard (advisory) — score {}/100\n\n",
                    docs.score
                ));
                out.push_str("| Rule | File | Detail |\n| --- | --- | --- |\n");
                for f in &docs.findings {
                    out.push_str(&format!(
                        "| `{}` | `{}` | {} |\n",
                        f.rule, f.file, f.message
                    ));
                }
                out.push('\n');
            }
        }

        if let Some(mcp) = &self.mcp {
            out.push_str(&format!(
                "### MCP conformance (`fail_on: {}`)\n\n",
                mcp.fail_on
            ));
            // Liveness first, above the per-check table. For a merge gate the failure a PR
            // most often introduces is not "answered wrongly", it is "stopped answering", and
            // a reader who has to derive that from two check ids has been told the wrong thing
            // first. Only rendered when there is something to say — a clean run's liveness is
            // in the coverage column, and an older prober reports none at all.
            for server in &mcp.servers {
                if let McpServerStatus::Probed {
                    liveness: Some(l), ..
                } = &server.status
                {
                    if l.notable() {
                        out.push_str(&format!("`{}`: {}\n\n", server.name, l.headline()));
                    }
                }
            }
            out.push_str("| Server | Result | Coverage |\n| --- | --- | --- |\n");
            for server in &mcp.servers {
                let (result, coverage) = match &server.status {
                    McpServerStatus::Probed {
                        tally, blocking, ..
                    } => (
                        if !blocking.is_empty() {
                            format!("**{} blocking**", blocking.len())
                        } else if tally.failed > 0 {
                            // "clear" next to "2 failed" reads as a contradiction
                            // unless the row says why: those failures are real,
                            // they are simply below the configured threshold.
                            format!("clear ({} failure(s) below the threshold)", tally.failed)
                        } else {
                            "clear".to_string()
                        },
                        // Say what the run did *not* establish, next to what it
                        // did. A 17-skip run must not read like a clean sweep.
                        format!(
                            "{} scored ({} passed, {} failed), {} skipped, {} errored",
                            tally.scored, tally.passed, tally.failed, tally.skipped, tally.errored
                        ),
                    ),
                    McpServerStatus::Unrunnable { .. } => {
                        ("**did not run**".to_string(), "—".to_string())
                    }
                };
                out.push_str(&format!(
                    "| `{}` | {} | {} |\n",
                    server.name, result, coverage
                ));
            }
            out.push('\n');
            for server in &mcp.servers {
                match &server.status {
                    McpServerStatus::Probed { blocking, .. } if !blocking.is_empty() => {
                        out.push_str(&format!("**`{}`** — failing checks:\n\n", server.name));
                        for b in blocking {
                            out.push_str(&format!(
                                "- `{}` ({}): {} — {}\n",
                                b.check,
                                b.severity.as_deref().unwrap_or("unclassified"),
                                b.reason,
                                b.detail.as_deref().unwrap_or("(no detail)")
                            ));
                        }
                        out.push('\n');
                    }
                    McpServerStatus::Unrunnable { reason } => {
                        out.push_str(&format!(
                            "**`{}`** — the lane could not run, so it blocks rather than \
                             reporting a pass it did not establish: {reason}\n\n",
                            server.name
                        ));
                    }
                    _ => {}
                }
            }
        }

        let slop_clear = self.slop.as_ref().is_none_or(|s| s.findings.is_empty());
        let security_clear = self.security.as_ref().is_none_or(|s| s.findings.is_empty());
        let convention_clear = self
            .convention
            .as_ref()
            .is_none_or(|s| s.findings.is_empty());
        let docs_clear = self.docs.as_ref().is_none_or(|s| s.findings.is_empty());
        let mcp_clear = self.mcp.as_ref().is_none_or(|m| m.all_clear());
        if self.survivors.is_empty()
            && self.zero_assertion_tests.is_empty()
            && slop_clear
            && security_clear
            && convention_clear
            && docs_clear
            && mcp_clear
        {
            // The MCP clause is conditional on the lane having *run*. Every
            // other lane here reads the diff and therefore always ran; claiming
            // "no MCP regressions" on a repo that declared no server would be
            // the gate taking credit for a check it never performed.
            out.push_str(
                "No surviving mutations, assertion-free tests, slop signatures, security anti-patterns, unknown imports, or documentation-standard findings on the changed surface.\n",
            );
            if self.mcp.is_some() {
                out.push_str("Every probed MCP server answered the negative-path sweep.\n");
            }
        }
        out
    }

    /// Render the report as pretty JSON for machine consumption.
    pub fn render_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    /// Heuristic for whether the zero-assertion check ran: it ran if there were
    /// changed files (the scan is keyed off them), even if it found nothing.
    fn check_ran_zero_assertion(&self) -> bool {
        !self.changed_rust_files.is_empty()
    }
}

/// One-line summary of the debt-delta: net score plus the per-proxy breakdown.
fn debt_line(d: &DebtDelta) -> String {
    format!(
        "net {:+} (complexity {:+}, duplication {:+}, coupling {:+})",
        d.score(),
        d.complexity,
        d.duplication,
        d.coupling
    )
}

/// One-line summary of the turnover lane for the text report.
fn turnover_line(t: &TurnoverLane) -> String {
    match t.status.as_str() {
        "skipped" => format!("skipped — {}", t.reason.as_deref().unwrap_or("")),
        status => format!(
            "{} ({}) — copy/paste {} · dup block {} · refactor {} · churn {} over {} added lines",
            status.to_uppercase(),
            t.mode,
            pct_opt(t.window.ratios.copy_paste),
            pct_opt(t.window.ratios.dup_block),
            pct_opt(t.window.ratios.refactor),
            pct_opt(t.window.ratios.churn),
            t.window.counts.added
        ),
    }
}

fn pct_opt(x: Option<f64>) -> String {
    match x {
        Some(v) => format!("{:.1}%", v * 100.0),
        None => "—".to_string(),
    }
}

/// Emoji + label badge for a severity tier, for the PR comment.
fn severity_badge(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "🟥 critical",
        Severity::High => "🟧 high",
        Severity::Medium => "🟨 medium",
        Severity::Low => "⬜ low",
    }
}

fn verdict_line(v: &Verdict) -> String {
    match v {
        Verdict::Pass => "PASS".to_string(),
        Verdict::Block { reasons } => format!("BLOCK — {}", reasons.join("; ")),
    }
}

fn preflight_line(p: &PreflightOutcome) -> String {
    match p {
        PreflightOutcome::Skipped => "skipped".to_string(),
        PreflightOutcome::Passed { runs } => format!("green & stable across {runs} run(s)"),
        PreflightOutcome::Failed { run, detail } => {
            format!("FAILED on run {run} — {detail}")
        }
        PreflightOutcome::Unstable { detail } => format!("UNSTABLE — {detail}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_gate::{McpGateReport, McpServerOutcome};
    use crate::pattern::{PatternFinding, PatternReport};

    fn survivor(file: &str, line: u32, desc: &str) -> Mutant {
        Mutant {
            file: file.into(),
            line,
            column: 5,
            function: None,
            description: desc.into(),
            name: format!("{file}:{line}:5: {desc}"),
        }
    }

    #[test]
    fn parses_canonical_name() {
        let m = Mutant::parse_name("src/foo.rs:12:5: replace > with >=").unwrap();
        assert_eq!(m.file, "src/foo.rs");
        assert_eq!(m.line, 12);
        assert_eq!(m.column, 5);
        assert_eq!(m.description, "replace > with >=");
    }

    #[test]
    fn parses_nested_path_with_no_description() {
        let m = Mutant::parse_name("a/b/c/lib.rs:1:1").unwrap();
        assert_eq!(m.file, "a/b/c/lib.rs");
        assert_eq!(m.line, 1);
        assert_eq!(m.column, 1);
        assert_eq!(m.description, "");
    }

    #[test]
    fn rejects_garbage() {
        assert!(Mutant::parse_name("").is_none());
        assert!(Mutant::parse_name("not a mutant").is_none());
        assert!(Mutant::parse_name("src/foo.rs:notanumber:5: x").is_none());
    }

    #[test]
    fn group_key_uses_function_then_file() {
        let mut m = Mutant::parse_name("src/foo.rs:12:5: replace x").unwrap();
        assert_eq!(m.group_key(), "src/foo.rs::<file>");
        m.function = Some("do_thing".into());
        assert_eq!(m.group_key(), "src/foo.rs::do_thing");
    }

    #[test]
    fn text_report_short_circuits_when_no_rust_changed() {
        let r = GateReport::new("main", "HEAD");
        let text = r.render_text();
        assert!(text.contains("nothing to mutate"));
        assert!(text.contains("verdict:    PASS"));
    }

    #[test]
    fn markdown_carries_marker_and_block_reasons() {
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.survivors = vec![survivor("src/x.rs", 10, "replace > with >=")];
        r.verdict = Verdict::Block {
            reasons: vec!["1 survivor(s) exceed the allowed 0".into()],
        };
        let md = r.render_markdown();
        assert!(md.starts_with(COMMENT_MARKER));
        assert!(md.contains("blocked"));
        assert!(md.contains("Why this is blocked"));
        assert!(md.contains("`src/x.rs:10:5`"));
    }

    #[test]
    fn markdown_orders_survivors_by_severity() {
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/auth.rs".into(), "src/util.rs".into()];
        r.survivors = vec![
            survivor("src/util.rs", 5, "replace + with -"), // medium
            survivor("src/auth.rs", 9, "replace > with >="), // critical
        ];
        let md = r.render_markdown();
        let crit = md.find("src/auth.rs:9:5").unwrap();
        let med = md.find("src/util.rs:5:5").unwrap();
        assert!(crit < med, "critical survivor must be listed first");
        assert!(md.contains("Severity"));
        assert!(md.contains("critical"));
    }

    #[test]
    fn debt_renders_only_when_nonzero() {
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.debt = Some(DebtDelta {
            complexity: 3,
            coupling: 1,
            ..DebtDelta::default()
        });
        assert!(r.render_markdown().contains("Debt-delta"));
        assert!(r.render_text().contains("net +4"));

        r.debt = Some(DebtDelta::default()); // score 0
        assert!(!r.render_markdown().contains("Debt-delta"));
    }

    #[test]
    fn markdown_clean_run_says_so() {
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        let md = r.render_markdown();
        assert!(md.contains("passed"));
        assert!(md.contains("No surviving mutations"));
    }

    fn slop_finding(rule: &str, file: &str, line: u32) -> PatternFinding {
        PatternFinding {
            rule: rule.into(),
            file: file.into(),
            line,
            message: format!("{rule} detected"),
            weight: 20,
        }
    }

    #[test]
    fn render_text_omits_slop_section_when_findings_empty() {
        // slop = Some but findings is empty — the "Slop signatures" section must
        // not appear (guards the `!slop.findings.is_empty()` check in render_text).
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.slop = Some(PatternReport::default());
        assert!(
            !r.render_text().contains("Slop signatures"),
            "render_text must not print the slop section when findings is empty"
        );
    }

    #[test]
    fn render_text_shows_slop_section_when_findings_present() {
        // With a real finding, the section must appear in text output.
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.slop = Some(PatternReport {
            findings: vec![slop_finding("redundant-wrapper", "src/x.rs", 1)],
            score: 25,
        });
        let text = r.render_text();
        assert!(text.contains("Slop signatures"));
        assert!(text.contains("redundant-wrapper"));
    }

    #[test]
    fn render_markdown_omits_slop_section_when_findings_empty() {
        // Guards the `!slop.findings.is_empty()` check in render_markdown.
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.slop = Some(PatternReport::default());
        assert!(
            !r.render_markdown().contains("Slop signatures"),
            "render_markdown must not print the slop section when findings is empty"
        );
    }

    #[test]
    fn render_markdown_shows_slop_section_when_findings_present() {
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.slop = Some(PatternReport {
            findings: vec![slop_finding("over-commented", "src/x.rs", 3)],
            score: 15,
        });
        let md = r.render_markdown();
        assert!(md.contains("Slop signatures"));
        assert!(md.contains("over-commented"));
    }

    #[test]
    fn markdown_clean_run_message_absent_when_slop_findings_present() {
        // No survivors and no zero-assertion tests, but slop findings exist.
        // The "No surviving mutations" clean-run message must NOT appear.
        // This guards both `&&` operators in the clean-run condition: if either
        // is replaced with `||`, the message would appear despite slop findings.
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.slop = Some(PatternReport {
            findings: vec![slop_finding("tautological-assert", "src/x.rs", 2)],
            score: 20,
        });
        assert!(
            !r.render_markdown().contains("No surviving mutations"),
            "clean-run message must not appear when slop findings are present"
        );
    }

    #[test]
    fn markdown_clean_run_message_present_when_slop_is_empty() {
        // slop = Some(empty) still counts as "clean" for the message.
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.slop = Some(PatternReport::default());
        assert!(
            r.render_markdown().contains("No surviving mutations"),
            "clean-run message must appear when slop is Some but findings is empty"
        );
    }

    fn security_finding(rule: &str, file: &str, line: u32) -> PatternFinding {
        PatternFinding {
            rule: rule.into(),
            file: file.into(),
            line,
            message: format!("{rule} detected"),
            weight: 25,
        }
    }

    #[test]
    fn render_text_shows_security_section_when_findings_present() {
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.security = Some(PatternReport {
            findings: vec![security_finding("hardcoded-secret", "src/x.rs", 5)],
            score: 40,
        });
        let text = r.render_text();
        assert!(
            text.contains("Security anti-patterns"),
            "render_text must print the security section when findings are present"
        );
        assert!(text.contains("hardcoded-secret"));
    }

    #[test]
    fn render_text_omits_security_section_when_findings_empty() {
        // security = Some but findings is empty — section must not appear.
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.security = Some(PatternReport::default());
        assert!(
            !r.render_text().contains("Security anti-patterns"),
            "render_text must not print the security section when findings is empty"
        );
    }

    #[test]
    fn render_markdown_shows_security_section_when_findings_present() {
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.security = Some(PatternReport {
            findings: vec![security_finding("weak-hash", "src/auth.rs", 10)],
            score: 25,
        });
        let md = r.render_markdown();
        assert!(
            md.contains("Security anti-patterns"),
            "render_markdown must include the security section when findings are present"
        );
        assert!(md.contains("weak-hash"));
        assert!(md.contains("src/auth.rs:10"));
    }

    #[test]
    fn markdown_clean_run_message_absent_when_security_findings_present() {
        // No survivors and no zero-assertion tests, but security findings exist.
        // The clean-run message must NOT appear.
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.security = Some(PatternReport {
            findings: vec![security_finding("sql-string-build", "src/x.rs", 3)],
            score: 25,
        });
        assert!(
            !r.render_markdown().contains("No surviving mutations"),
            "clean-run message must not appear when security findings are present"
        );
    }

    fn convention_finding(rule: &str, file: &str, line: u32) -> PatternFinding {
        PatternFinding {
            rule: rule.into(),
            file: file.into(),
            line,
            message: format!("{rule} detected"),
            weight: 30,
        }
    }

    #[test]
    fn render_text_omits_convention_section_when_findings_empty() {
        // convention = Some but findings is empty — the section must not appear.
        // Guards the `!conv.findings.is_empty()` check in render_text.
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.convention = Some(PatternReport::default());
        assert!(
            !r.render_text().contains("Convention"),
            "render_text must not print the convention section when findings is empty"
        );
    }

    #[test]
    fn render_text_shows_convention_section_when_findings_present() {
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.convention = Some(PatternReport {
            findings: vec![convention_finding("unknown-crate-import", "src/x.rs", 5)],
            score: 30,
        });
        let text = r.render_text();
        assert!(
            text.contains("Convention"),
            "render_text must print the convention section when findings are present"
        );
        assert!(text.contains("unknown-crate-import"));
    }

    #[test]
    fn render_markdown_omits_convention_section_when_findings_empty() {
        // Guards the `!conv.findings.is_empty()` check in render_markdown.
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.convention = Some(PatternReport::default());
        assert!(
            !r.render_markdown().contains("Convention"),
            "render_markdown must not print the convention section when findings is empty"
        );
    }

    #[test]
    fn render_markdown_shows_convention_section_when_findings_present() {
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.convention = Some(PatternReport {
            findings: vec![convention_finding(
                "unknown-crate-import",
                "src/auth.rs",
                10,
            )],
            score: 30,
        });
        let md = r.render_markdown();
        assert!(
            md.contains("Convention"),
            "render_markdown must include the convention section when findings are present"
        );
        assert!(md.contains("unknown-crate-import"));
        assert!(md.contains("src/auth.rs:10"));
    }

    #[test]
    fn markdown_clean_run_message_absent_when_convention_findings_present() {
        // No survivors and no zero-assertion tests, but convention findings exist.
        // The clean-run message must NOT appear.
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.convention = Some(PatternReport {
            findings: vec![convention_finding("unknown-crate-import", "src/x.rs", 3)],
            score: 30,
        });
        assert!(
            !r.render_markdown().contains("No surviving mutations"),
            "clean-run message must not appear when convention findings are present"
        );
    }

    fn docs_finding(rule: &str, file: &str) -> PatternFinding {
        PatternFinding {
            rule: rule.into(),
            file: file.into(),
            line: 0, // module-level findings — see docs_gate::finding
            message: format!("{rule} detected"),
            weight: 25,
        }
    }

    #[test]
    fn render_text_shows_docs_summary_line_when_present() {
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.docs = Some(PatternReport {
            findings: vec![docs_finding("docs-missing-readme", "mods/example")],
            score: 40,
        });
        let text = r.render_text();
        assert!(
            text.contains("docs:"),
            "render_text must print the docs summary line when the lane ran"
        );
        assert!(text.contains("docs-missing-readme"));
    }

    #[test]
    fn render_text_omits_docs_section_when_findings_empty() {
        // docs = Some but findings is empty — the "Documentation standard"
        // section must not appear. Guards the `!docs.findings.is_empty()` check
        // in render_text.
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.docs = Some(PatternReport::default());
        assert!(
            !r.render_text().contains("Documentation standard"),
            "render_text must not print the docs section when findings is empty"
        );
    }

    #[test]
    fn render_markdown_shows_docs_section_when_findings_present() {
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.docs = Some(PatternReport {
            findings: vec![docs_finding("docs-missing-readme", "mods/example")],
            score: 40,
        });
        let md = r.render_markdown();
        assert!(
            md.contains("Documentation standard"),
            "render_markdown must include the docs section when findings are present"
        );
        assert!(md.contains("docs-missing-readme"));
    }

    #[test]
    fn render_markdown_omits_docs_section_when_findings_empty() {
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.docs = Some(PatternReport::default());
        assert!(
            !r.render_markdown().contains("Documentation standard"),
            "render_markdown must not print the docs section when findings is empty"
        );
    }

    #[test]
    fn markdown_clean_run_message_absent_when_only_docs_findings_present() {
        // A PR that trips ONLY the docs lane must not get the false "all clear"
        // message — that was the bug: docs_clear wasn't part of the gate.
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.docs = Some(PatternReport {
            findings: vec![docs_finding("docs-missing-readme", "mods/example")],
            score: 40,
        });
        assert!(
            !r.render_markdown().contains("No surviving mutations"),
            "clean-run message must not appear when docs findings are present"
        );
    }

    fn mcp_probed(name: &str, blocking: &[&str], skipped: usize) -> McpServerOutcome {
        crate::mcp_gate::McpServerOutcome {
            name: name.into(),
            status: McpServerStatus::Probed {
                tally: crate::mcp_gate::McpTally {
                    scored: 34 - skipped,
                    passed: 34 - skipped - blocking.len(),
                    failed: blocking.len(),
                    errored: 0,
                    skipped,
                },
                blocking: blocking
                    .iter()
                    .map(|id| crate::mcp_gate::McpBlockingCheck {
                        check: (*id).to_string(),
                        severity: Some("critical".into()),
                        outcome: "fail".into(),
                        reason: "failed a Critical check".into(),
                        detail: Some("stopped answering after a truncated frame".into()),
                    })
                    .collect(),
                liveness: None,
                suite_version: Some("specprobe/0.1.0".into()),
            },
        }
    }

    fn report_with_mcp(servers: Vec<McpServerOutcome>) -> GateReport {
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        r.mcp = Some(McpGateReport {
            fail_on: "critical".into(),
            servers,
        });
        r
    }

    #[test]
    fn render_text_names_the_failing_check_and_what_was_seen() {
        let r = report_with_mcp(vec![mcp_probed("acme", &["NP-FRAME-001"], 17)]);
        let text = r.render_text();
        assert!(text.contains("mcp:"), "{text}");
        assert!(text.contains("NP-FRAME-001"), "{text}");
        assert!(text.contains("truncated frame"), "{text}");
    }

    #[test]
    fn render_text_states_the_skips_rather_than_hiding_them() {
        // A 17-skip run must not present as a fully-exercised one. This is the
        // same defect the admission board had, and it must not be reintroduced
        // on the surface a PR author actually reads.
        let r = report_with_mcp(vec![mcp_probed("acme", &[], 17)]);
        let text = r.render_text();
        assert!(text.contains("17 skipped"), "{text}");
    }

    #[test]
    fn render_markdown_has_a_table_and_the_failing_detail() {
        let r = report_with_mcp(vec![mcp_probed("acme", &["NP-LIFE-001"], 17)]);
        let md = r.render_markdown();
        assert!(md.contains("### MCP conformance"), "{md}");
        assert!(md.contains("`NP-LIFE-001`"), "{md}");
        assert!(md.contains("17 skipped"), "{md}");
        assert!(md.contains("1 blocking"), "{md}");
    }

    #[test]
    fn render_markdown_says_a_lane_did_not_run_rather_than_omitting_it() {
        let r = report_with_mcp(vec![McpServerOutcome {
            name: "acme".into(),
            status: McpServerStatus::Unrunnable {
                reason: "could not run `specprobe`".into(),
            },
        }]);
        let md = r.render_markdown();
        assert!(md.contains("did not run"), "{md}");
        assert!(md.contains("could not run `specprobe`"), "{md}");
    }

    #[test]
    fn the_all_clear_line_does_not_appear_when_the_mcp_lane_blocked() {
        // The exact failure mode this guard exists for: a PR whose ONLY problem
        // is a broken MCP server, told "no findings on the changed surface".
        let r = report_with_mcp(vec![mcp_probed("acme", &["NP-FRAME-001"], 17)]);
        let md = r.render_markdown();
        assert!(
            !md.contains("No surviving mutations,"),
            "the all-clear must not appear alongside an MCP regression:\n{md}"
        );
    }

    #[test]
    fn a_below_threshold_failure_is_reported_not_hidden_behind_clear() {
        // A run can be "clear" and still have found real failures — they were
        // below the threshold. Printing a bare "clear" next to "2 failed" reads
        // as a contradiction, and hiding the count would be worse.
        let mut s = mcp_probed("acme", &[], 17);
        if let McpServerStatus::Probed { tally, .. } = &mut s.status {
            tally.failed = 2;
            tally.passed -= 2;
        }
        let md = report_with_mcp(vec![s]).render_markdown();
        assert!(
            md.contains("clear (2 failure(s) below the threshold)"),
            "{md}"
        );
    }

    #[test]
    fn a_clean_mcp_run_still_allows_the_all_clear_line() {
        let r = report_with_mcp(vec![mcp_probed("acme", &[], 17)]);
        let md = r.render_markdown();
        assert!(md.contains("No surviving mutations,"), "{md}");
        assert!(md.contains("Every probed MCP server answered"), "{md}");
        // ...and the clean run is still reported, so "all clear" is legible as
        // "we probed it and it was fine", not "we never looked".
        assert!(md.contains("### MCP conformance"), "{md}");
    }

    #[test]
    fn a_report_without_the_lane_renders_no_mcp_section_at_all() {
        let mut r = GateReport::new("main", "HEAD");
        r.changed_rust_files = vec!["src/x.rs".into()];
        assert!(!r.render_text().contains("mcp:"));
        let md = r.render_markdown();
        assert!(!md.contains("### MCP conformance"), "{md}");
        // And the all-clear line must not claim an MCP result either — the lane
        // did not run, so there is nothing to be clear about.
        assert!(!md.contains("Every probed MCP server"), "{md}");
    }

    #[test]
    fn the_mcp_lane_round_trips_through_the_json_artifact() {
        let r = report_with_mcp(vec![mcp_probed("acme", &["NP-FRAME-001"], 17)]);
        let back: GateReport = serde_json::from_str(&r.render_json()).unwrap();
        assert_eq!(back.mcp, r.mcp);
    }

    fn dead_liveness() -> crate::mcp_gate::McpLiveness {
        crate::mcp_gate::McpLiveness {
            exchanges: 34,
            timed_out: 12,
            crashed: 0,
            wedges: 3,
            recovered: 0,
            restarts: 2,
            ended_down: true,
            worst_latency_ms: 119,
        }
    }

    fn healthy_liveness() -> crate::mcp_gate::McpLiveness {
        crate::mcp_gate::McpLiveness {
            exchanges: 34,
            timed_out: 7,
            worst_latency_ms: 250,
            ..Default::default()
        }
    }

    fn with_liveness(mut o: McpServerOutcome, l: crate::mcp_gate::McpLiveness) -> McpServerOutcome {
        if let McpServerStatus::Probed { liveness, .. } = &mut o.status {
            *liveness = Some(l);
        }
        o
    }

    #[test]
    fn a_hang_is_the_first_thing_the_comment_says() {
        // "failed NP-FRAME-001 and NP-FRAME-005" is accurate and tells a PR author almost
        // nothing. The same fact in the words they will act on goes above the table.
        let r = report_with_mcp(vec![with_liveness(
            mcp_probed("acme", &["NP-FRAME-001"], 17),
            dead_liveness(),
        )]);
        let md = r.render_markdown();
        let liveness_at = md
            .find("stopped answering and never came back")
            .expect(md.as_str());
        let table_at = md.find("| Server | Result").unwrap();
        assert!(liveness_at < table_at, "liveness must come first:\n{md}");
        // ...and the block reason itself leads with it, not with the check ids. The verdict is
        // not computed by `report_with_mcp`, so assert on the outcome that feeds it.
        let reason = r.mcp.as_ref().unwrap().servers[0].block_reason().unwrap();
        assert!(
            reason.starts_with("MCP server `acme` stopped answering"),
            "{reason}"
        );
    }

    #[test]
    fn a_healthy_server_gets_no_liveness_paragraph() {
        // A clean run's liveness is already in the coverage column. Repeating it above the table
        // would make the one place reserved for "something went wrong" mean nothing.
        let r = report_with_mcp(vec![with_liveness(
            mcp_probed("acme", &[], 17),
            healthy_liveness(),
        )]);
        let md = r.render_markdown();
        assert!(!md.contains("stayed answerable across all"), "{md}");
        assert!(md.contains("### MCP conformance"), "{md}");
    }

    #[test]
    fn an_older_prober_that_reports_no_liveness_gets_no_liveness_claim() {
        // Absent means nobody measured, never "stayed up". Rendering a reassuring line here
        // would be the gate taking credit for an observation it did not have.
        let r = report_with_mcp(vec![mcp_probed("acme", &["NP-FRAME-001"], 17)]);
        let md = r.render_markdown();
        assert!(!md.contains("stayed answerable"), "{md}");
        // No liveness paragraph. (The check's own detail may say "stopped answering" — that is
        // the probe quoting what it saw, not the gate claiming a measurement it does not have.)
        assert!(!md.contains("`acme`: "), "{md}");
        assert!(!md.contains("never came back"), "{md}");
        assert!(md.contains("NP-FRAME-001"), "{md}");
    }

    #[test]
    fn render_text_carries_the_liveness_line_too() {
        let r = report_with_mcp(vec![with_liveness(
            mcp_probed("acme", &["NP-FRAME-001"], 17),
            dead_liveness(),
        )]);
        let text = r.render_text();
        assert!(text.contains("liveness:"), "{text}");
        assert!(text.contains("never came back"), "{text}");
        // Markdown emphasis must not leak into the plain-text job log.
        assert!(!text.contains("**"), "{text}");
    }
}
