//! turnover-core — the signal model behind the maintainability gate. PURE: no git, no
//! filesystem, no clock, no parser runtime. Everything in here is a function of the text on
//! both sides of a change plus a configuration, and that is deliberate: a classification bug
//! is the only way the gate can fail a build for the wrong reason, so this surface has to stay
//! auditable line-by-line and testable with nothing but strings.
//!
//! The crate answers four questions about one commit, then two about a run of commits:
//!
//!   * [`signals::classify`] — for one commit, how many *significant* added lines were
//!     **moved** (refactoring), **copy/pasted**, part of a **duplicated block**, and how many
//!     lines were added at all. These are GitClear's published line-operation buckets, kept
//!     to the three the dossier calls "mechanically unambiguous", plus deletions.
//!   * [`churn::attribute`] — across commits in time order, which added lines were removed
//!     again within the churn horizon (two weeks, by the published definition).
//!   * [`window::aggregate`] — turn per-commit counts over a time range into ratios that are
//!     comparable across repositories and languages, because every denominator is
//!     *significant added lines* (comments, blanks and punctuation-only lines never count).
//!   * [`policy::evaluate`] — compare a trailing window against the repository's own baseline
//!     and decide pass / fail / insufficient-sample. The gate is the product; the ratios are
//!     just its inputs.
//!
//! Language awareness enters through [`line::Mask`]: the caller (normally `turnover-lang`,
//! tree-sitter backed) says which lines are significant; the fallback heuristic in
//! [`line::heuristic_mask`] keeps the model usable for languages without a grammar.

pub mod attribution;
pub mod baseline;
pub mod churn;
pub mod language;
pub mod line;
pub mod policy;
pub mod signals;
pub mod tokens;
pub mod window;

pub use attribution::{AttributionConfig, Origin};
pub use baseline::Baseline;
pub use language::Language;
pub use policy::{Policy, Verdict};
pub use signals::{classify, CommitInput, CommitSignals, Counts, FileChange, SignalConfig};
pub use window::{aggregate, Aggregate, Ratios};
