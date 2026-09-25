//! `turnover.toml`: the policy (gate / thresholds / drift) plus the classifier, churn and
//! walk settings. Every section and every key is optional; the defaults are the documented
//! ones, so an empty file is a valid, sensible configuration.

use serde::{Deserialize, Serialize};
use turnover_core::attribution::AttributionConfig;
use turnover_core::churn::DEFAULT_HORIZON_SECS;
use turnover_core::policy::Policy;
use turnover_core::signals::SignalConfig;
use turnover_history::WalkOptions;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(flatten)]
    pub policy: Policy,
    pub signals: SignalConfig,
    pub churn: ChurnConfig,
    pub walk: WalkConfig,
    /// What marks a commit as AI-coauthored (`[attribution]`).
    pub attribution: AttributionConfig,
    /// How many of the judged commits `gate` explains file by file for the report's
    /// "where it comes from" list (`[report] explain_commits`). 0 disables it.
    pub report: ReportConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReportConfig {
    /// Cap on commits explained per gate run — a PR is a handful, a trailing window can be
    /// hundreds, and explaining is a second parse of every touched file.
    pub explain_commits: usize,
    /// Files listed under "where it comes from".
    pub offenders: usize,
}

impl Default for ReportConfig {
    fn default() -> Self {
        ReportConfig {
            explain_commits: 40,
            offenders: 8,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChurnConfig {
    /// Days within which a deleted line counts as churn of its addition.
    pub horizon_days: u32,
}

impl Default for ChurnConfig {
    fn default() -> Self {
        ChurnConfig {
            horizon_days: (DEFAULT_HORIZON_SECS / 86_400) as u32,
        }
    }
}

impl ChurnConfig {
    pub fn horizon_secs(&self) -> i64 {
        i64::from(self.horizon_days) * 86_400
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WalkConfig {
    pub first_parent: bool,
    pub skip_merges: bool,
    pub max_file_bytes: usize,
    pub include_other: bool,
    pub ignore_paths: Vec<String>,
}

impl Default for WalkConfig {
    fn default() -> Self {
        let d = WalkOptions::default();
        WalkConfig {
            first_parent: d.first_parent,
            skip_merges: d.skip_merges,
            max_file_bytes: d.max_file_bytes,
            include_other: d.include_other,
            ignore_paths: d.ignore_paths,
        }
    }
}

impl WalkConfig {
    pub fn to_options(&self) -> WalkOptions {
        WalkOptions {
            first_parent: self.first_parent,
            skip_merges: self.skip_merges,
            max_file_bytes: self.max_file_bytes,
            include_other: self.include_other,
            ignore_paths: self.ignore_paths.clone(),
            ..WalkOptions::default()
        }
    }
}

impl Config {
    /// Walk options with this config's attribution rules attached.
    pub fn walk_options(&self) -> WalkOptions {
        let mut o = self.walk.to_options();
        o.attribution = self.attribution.clone();
        o
    }
}

impl Config {
    pub fn load(path: Option<&std::path::Path>) -> anyhow::Result<Config> {
        match path {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| anyhow::anyhow!("read config {}: {e}", p.display()))?;
                toml::from_str(&text)
                    .map_err(|e| anyhow::anyhow!("parse config {}: {e}", p.display()))
            }
            None => {
                let default = std::path::Path::new("turnover.toml");
                if default.exists() {
                    Config::load(Some(default))
                } else {
                    Ok(Config::default())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_partial_configs_parse() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.policy.gate.window_days, 90);
        assert_eq!(c.churn.horizon_days, 14);
        assert!(c.walk.skip_merges);
        let c: Config = toml::from_str("[gate]\nwindow_days = 30\n[signals]\nblock_min_lines = 8\n[walk]\nignore_paths = [\"gen\"]\n[churn]\nhorizon_days = 30\n[attribution]\nai_markers = [\"[bot]\"]\n[report]\nexplain_commits = 5\n").unwrap();
        assert_eq!(c.attribution.ai_markers, vec!["[bot]".to_string()]);
        assert!(
            !c.attribution.ai_emails.is_empty(),
            "unset lists keep their defaults"
        );
        assert_eq!(c.report.explain_commits, 5);
        assert_eq!(
            c.walk_options().attribution.ai_markers,
            vec!["[bot]".to_string()]
        );
        assert_eq!(c.policy.gate.window_days, 30);
        assert_eq!(c.signals.block_min_lines, 8);
        assert_eq!(c.walk.ignore_paths, vec!["gen".to_string()]);
        assert_eq!(c.churn.horizon_secs(), 30 * 86_400);
    }
}
