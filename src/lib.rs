// SPDX-License-Identifier: Apache-2.0
//! Mergestro Gate — behavioral merge gate (the slop half of Mergestro).
//!
//! A differential mutation gate for Rust: it surfaces real survivors the test
//! suite passed over, on the changed surface only. Phase 1 proved the catch as
//! an advisory script; **Phase 2 made it an actual gate** — pure-Rust diffing
//! against the base ref, a blocking verdict, a free zero-assertion pre-check,
//! and an idempotent PR comment — packaged as an installable GitHub Action.
//! **Phase 3 adds validation telemetry** ([`metrics`]) and an analyser
//! ([`analyze`]) so the catch can be measured across real usage. **Phase 4
//! starts the rollout**: survivors are ranked by [`severity`] (so the dangerous
//! ones surface first and can gate on their own), and a [`debt`]-delta budget
//! turns the point-in-time check into a trajectory with burn-rate framing.
//!
//! Pipeline (see [`pipeline::run`]):
//!
//! 1. **Diff** the head ref against the base ref ([`diff`], via `gix`). Rust-only.
//! 2. **Debt-delta** ([`debt`]): net complexity/duplication/coupling added by the
//!    diff, framed against a budget.
//! 3. **Determinism pre-flight** ([`preflight`]): the suite must be green and
//!    stable before mutation results mean anything.
//! 4. **Mutate** the changed surface only via `cargo-mutants --in-diff`
//!    ([`mutants`]), with a per-function mutant cap to bound runtime.
//! 5. **Zero-assertion pre-check** ([`zero_assertion`]): a static second signal.
//! 6. **MCP lane** ([`mcp_gate`]), when the repo declares a first-party MCP
//!    server the diff touches: build it, probe it over stdio with `specprobe`,
//!    and block on a regression that would deny the server admission. This is
//!    the one lane that runs the artifact rather than reading the diff.
//! 7. **Verdict** ([`verdict`]): hard gates block (survivors over budget, a
//!    severity tier, debt over budget, or an MCP regression), soft signals
//!    score.
//! 8. **Report** ([`report`]) as text, JSON, or a Markdown PR comment
//!    ([`github`]), survivors ordered by [`severity`]; emit a run record for
//!    validation ([`metrics`]).
//!
//! Command execution goes through the [`runner::CommandRunner`] trait so the
//! orchestration logic stays testable.
//!
//! Alongside the gate, [`progression`] answers the question a single gate run
//! cannot: where the repository stands against the plan it wrote down. It
//! resolves an authored milestone tree against the repo's own commits and PRs
//! and renders it — as the SVG a README carries, and as the snapshot the
//! Mergestro console draws on its canvas.

pub mod analyze;
pub mod config;
pub mod convention;
pub mod debt;
pub mod diff;
pub mod docs_gate;
pub mod engine;
pub mod estimate;
pub mod github;
pub mod golang;
pub mod js;
pub mod jvm;
pub mod mcp_gate;
pub mod metrics;
pub mod mutants;
pub mod pattern;
pub mod pipeline;
pub mod preflight;
pub mod progression;
pub mod python;
pub mod report;
pub mod runner;
pub mod sarif;
pub mod security;
pub mod severity;
pub mod slop;
pub mod turnover_lane;
pub mod verdict;
pub mod weakened_tests;
pub mod zero_assertion;
