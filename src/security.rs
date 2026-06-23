// SPDX-License-Identifier: Apache-2.0
//! Pattern lane — **security anti-patterns** (Track B, lane 2).
//!
//! Static, diff-scoped, advisory — and it reuses the shared [`PatternReport`]
//! plumbing, so findings render and trend exactly like the slop lane. Rules are
//! conservative, line-oriented regexes over the diff's **added** lines (the
//! new-side content), tuned to keep false positives low. This is a focused
//! lane that catches a few high-signal mistakes agents make — **not** a SAST.
//!
//! Detectors:
//! - **hardcoded-secret** — a credential-shaped identifier assigned a string
//!   literal (skipped when the line reads from the environment).
//! - **weak-hash** — MD5 / SHA-1 used where a security hash is implied.
//! - **sql-string-build** — a `format!` SQL string (use parameterized queries).
//! - **shell-command** — spawning a shell (`sh -c` etc.), a command-injection risk.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use crate::pattern::{PatternFinding, PatternReport};

struct Rule {
    id: &'static str,
    weight: u32,
    re: Regex,
    message: &'static str,
}

/// The rule set, compiled once.
fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        vec![
            Rule {
                id: "hardcoded-secret",
                weight: 40,
                re: Regex::new(
                    r#"(?i)\b(password|passwd|secret|api[_-]?key|access[_-]?key|client[_-]?secret|auth[_-]?token|token)\b\s*[:=]\s*"[^"]{6,}""#,
                )
                .expect("valid regex"),
                message: "hardcoded credential/secret literal — load it from the environment or a secret store",
            },
            Rule {
                id: "weak-hash",
                weight: 25,
                re: Regex::new(r"(?i)\b(md5|sha1)\b").expect("valid regex"),
                message: "weak hash (MD5/SHA-1) — use SHA-256+ for any security purpose",
            },
            Rule {
                id: "sql-string-build",
                weight: 25,
                re: Regex::new(
                    r#"(?i)format!\s*\(\s*"[^"]*\b(select|insert|update|delete|drop)\b"#,
                )
                .expect("valid regex"),
                message: "SQL assembled by string interpolation — use parameterized queries",
            },
            Rule {
                id: "shell-command",
                weight: 20,
                re: Regex::new(
                    r#"Command::new\(\s*"(sh|bash|/bin/sh|/bin/bash|zsh|cmd|powershell)""#,
                )
                .expect("valid regex"),
                message: "spawning a shell — command-injection risk; exec the program directly without a shell",
            },
        ]
    })
}

/// Lines that read from the environment aren't hardcoded secrets — don't flag.
///
/// Inline comments are stripped first so `// move to env later` or a variable
/// named `envelope` can't accidentally suppress a real finding.
fn reads_env(line: &str) -> bool {
    static ENV_READ: OnceLock<Regex> = OnceLock::new();
    // Strip any trailing `// …` comment before testing.
    let code = line.split("//").next().unwrap_or(line);
    ENV_READ
        .get_or_init(|| {
            Regex::new(
                r"(?i)\b(std::)?env::(var|var_os)\b|\bgetenv\s*\(|\bfrom_env\b|\bdotenv(?:::\w+)?\b|\bsecret_store\b",
            )
            .expect("valid regex")
        })
        .is_match(code)
}

/// Scan changed Rust files for security anti-patterns on the added lines.
/// Best-effort: unreadable files and untracked files are skipped silently.
pub fn scan_files(
    repo: &Path,
    files: &[String],
    added: &BTreeMap<String, BTreeSet<u32>>,
) -> PatternReport {
    let mut findings = Vec::new();
    for file in files {
        let Some(added_lines) = added.get(file) else {
            continue;
        };
        if added_lines.is_empty() {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(repo.join(file)) else {
            continue;
        };
        let lines: Vec<&str> = src.lines().collect();
        for &n in added_lines {
            let Some(text) = lines.get((n as usize).saturating_sub(1)) else {
                continue;
            };
            // Skip comment lines — a keyword in a comment isn't a vuln.
            if text.trim_start().starts_with("//") {
                continue;
            }
            for rule in rules() {
                if !rule.re.is_match(text) {
                    continue;
                }
                if rule.id == "hardcoded-secret" && reads_env(text) {
                    continue;
                }
                findings.push(PatternFinding {
                    rule: rule.id.to_string(),
                    file: file.to_string(),
                    line: n,
                    message: rule.message.to_string(),
                    weight: rule.weight,
                });
            }
        }
    }
    PatternReport::from_findings(findings)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scan a single source string, treating every line as added.
    fn scan(src: &str) -> PatternReport {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.rs"), src).unwrap();
        let added: BTreeSet<u32> = (1..=src.lines().count() as u32).collect();
        let map: BTreeMap<String, BTreeSet<u32>> = [("x.rs".to_string(), added)].into();
        scan_files(dir.path(), &["x.rs".to_string()], &map)
    }

    fn rules_hit(r: &PatternReport) -> Vec<&str> {
        r.findings.iter().map(|f| f.rule.as_str()).collect()
    }

    #[test]
    fn flags_hardcoded_secret() {
        let r = scan("let api_key = \"sk-abcdef123456\";\n");
        assert!(rules_hit(&r).contains(&"hardcoded-secret"));
    }

    #[test]
    fn does_not_flag_env_read() {
        let r = scan("let api_key = std::env::var(\"API_KEY\").unwrap();\n");
        assert!(!rules_hit(&r).contains(&"hardcoded-secret"));
    }

    #[test]
    fn still_flags_hardcoded_secret_when_comment_mentions_env() {
        // "// move to env later" must not suppress the finding.
        let r = scan("let token = \"abcdef123456\"; // move to env later\n");
        assert!(rules_hit(&r).contains(&"hardcoded-secret"));
    }

    #[test]
    fn still_flags_hardcoded_secret_when_variable_named_envelope() {
        // "envelope" contains "env" as a substring — must not suppress.
        let r = scan("let secret = \"hunter2-secret\"; let envelope = true;\n");
        assert!(rules_hit(&r).contains(&"hardcoded-secret"));
    }

    #[test]
    fn does_not_flag_hardcoded_secret_when_env_read_on_same_line() {
        // A struct literal where the line both triggers the hardcoded-secret
        // pattern (token: "...") and contains an env-read (env::var). The
        // reads_env suppression must fire. If reads_env always returned false
        // this test would fail.
        let r =
            scan("let c = Config { token: \"hardcoded-fallback\", key: env::var(\"K\").ok() };\n");
        assert!(
            !rules_hit(&r).contains(&"hardcoded-secret"),
            "hardcoded-secret must be suppressed when the same line also has an env read"
        );
    }

    #[test]
    fn reads_env_does_not_suppress_non_secret_rules() {
        // reads_env suppression must ONLY apply to hardcoded-secret (rule.id ==
        // check). A line with md5 and an env-read must still emit weak-hash. If
        // the == is flipped to != every non-secret rule would be suppressed.
        let r = scan("use md5; let h = std::env::var(\"DATA\").unwrap();\n");
        assert!(
            rules_hit(&r).contains(&"weak-hash"),
            "weak-hash must fire even when the line also reads from the environment"
        );
    }

    #[test]
    fn does_not_flag_short_or_type_annotated() {
        // No literal assigned (type annotation only) → no match.
        let r = scan("struct C { password: String }\n");
        assert!(!rules_hit(&r).contains(&"hardcoded-secret"));
    }

    #[test]
    fn flags_weak_hash() {
        let r = scan("use md5::Md5;\nlet h = Sha1::new();\n");
        assert!(rules_hit(&r).contains(&"weak-hash"));
    }

    #[test]
    fn flags_sql_string_build() {
        let r = scan("let q = format!(\"SELECT * FROM users WHERE id = {}\", id);\n");
        assert!(rules_hit(&r).contains(&"sql-string-build"));
    }

    #[test]
    fn does_not_flag_parameterized_sql() {
        let r = scan("let q = \"SELECT * FROM users WHERE id = $1\";\n");
        assert!(!rules_hit(&r).contains(&"sql-string-build"));
    }

    #[test]
    fn flags_shell_command() {
        let r = scan("let out = Command::new(\"sh\").arg(\"-c\").arg(cmd).output();\n");
        assert!(rules_hit(&r).contains(&"shell-command"));
    }

    #[test]
    fn does_not_flag_direct_exec() {
        let r = scan("let out = Command::new(\"git\").arg(\"status\").output();\n");
        assert!(r.findings.is_empty());
    }

    #[test]
    fn ignores_comment_lines() {
        let r = scan("// let api_key = \"sk-abcdef123456\"; example\n");
        assert!(r.findings.is_empty());
    }

    #[test]
    fn score_reflects_weights() {
        let r = scan("let secret = \"hunter2-very-secret\";\n");
        // one hardcoded-secret finding, weight 40
        assert_eq!(r.score, 40);
    }
}
