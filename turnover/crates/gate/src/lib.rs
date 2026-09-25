//! turnover-gate — the gate as a library.
//!
//! Everything a front-end needs to turn a repository plus a policy into a verdict lives
//! here, so the `turnover` binary and the turnover lane inside the Mergestro gate call the
//! same functions and cannot disagree about what a failure is:
//!
//!   * [`config::Config`] — the TOML policy (gate / thresholds / drift / signals / churn /
//!     walk), every key optional.
//!   * [`run::build_baseline`] / [`run::run_gate`] — the baseline on disk (build once,
//!     refresh incrementally) and the evaluation of a trailing window or a PR scope
//!     against it.
//!   * [`render`] — the text report and the Markdown section a PR comment carries.
//!   * [`record`] — the `turnover` record on Mergestro's ingest wire.
//!   * [`remote`] — the baseline-service client: fetch the plane's copy before a run, push
//!     it back after a refresh.
//!
//! No decision is made in a front-end: they parse arguments, call in, print.

pub mod config;
pub mod record;
pub mod remote;
pub mod render;
pub mod run;

pub use config::Config;
pub use run::{
    build_baseline, run_gate, BaselineRequest, BaselineSummary, GateError, GateOutcome,
    GateRequest, Scope,
};
