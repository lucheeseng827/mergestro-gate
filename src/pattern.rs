// SPDX-License-Identifier: Apache-2.0
//! Shared types for the pattern-checker lanes (Track B).
//!
//! Each lane — AI-slop signatures ([`crate::slop`]), security anti-patterns
//! ([`crate::security`]), and later convention RAG — is a static, diff-scoped
//! scan that emits [`PatternFinding`]s and rolls them into a 0–100
//! [`PatternReport`] score. They share this shape so the report rendering and
//! the telemetry handle every lane the same way.

use serde::{Deserialize, Serialize};

/// One pattern hit on the changed surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternFinding {
    /// Stable rule id (e.g. `"redundant-wrapper"`, `"hardcoded-secret"`).
    pub rule: String,
    pub file: String,
    pub line: u32,
    pub message: String,
    /// Contribution to the lane's score.
    pub weight: u32,
}

/// A lane's result for one run: the findings plus a saturating 0–100 score.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternReport {
    pub findings: Vec<PatternFinding>,
    pub score: u32,
}

impl PatternReport {
    /// Build a report from findings, scoring as the weight sum capped at 100.
    pub fn from_findings(findings: Vec<PatternFinding>) -> Self {
        let score = findings.iter().map(|f| f.weight).sum::<u32>().min(100);
        PatternReport { findings, score }
    }
}
