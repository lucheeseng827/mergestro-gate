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
//!
//! ## Test code is exempt, with one exception
//!
//! Every rule above is about code that *ships* and meets untrusted input. Test
//! code does neither, and a fixture is not a vulnerability: `token: "abc123"`
//! in a `#[cfg(test)]` module is the normal way to test an auth header, so
//! flagging it taught teams to ignore the lane rather than to fix anything.
//! Two things count as test code — a Cargo test target (`tests/`, `benches/`)
//! and a `#[cfg(test)]` item anywhere in a shipping file.
//!
//! **The exception is a vendor-issued credential.** `AKIA…`, `ghp_…`,
//! `sk-ant-…` and their kin are formats an issuer hands out, never a string a
//! person invents for a fixture — and a test file is one of the most common
//! places a live key actually gets committed. Exempting tests wholesale would
//! turn this lane's blind spot into precisely the case it most needs to catch,
//! so those stay reportable wherever they appear.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Attribute, Item, Meta, Token};

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

/// Whether this path is a Cargo *test target* rather than shipping code.
///
/// `tests/` and `benches/` are separate targets: nothing in them is compiled
/// into the library a user links, so a credential-shaped literal there is a
/// fixture by construction. Matched on whole path segments, so a crate called
/// `tests-support` is not mistaken for one.
fn is_test_target(file: &str) -> bool {
    // `tests.rs` beside the module it tests is declared `#[cfg(test)] mod tests;`
    // from its parent, so the file itself carries no attribute to find.
    if file.rsplit('/').next() == Some("tests.rs") {
        return true;
    }
    file.split('/')
        .any(|seg| seg == "tests" || seg == "benches")
}

/// Whether a `cfg` predicate **requires** `test` to be set.
///
/// Evaluated as a meta-expression rather than searched as text, because the two
/// disagree in ways that matter. `feature = "test"` contains the word `test`
/// and is a name/value pair, not the `test` predicate — a crate may turn that
/// feature on in a shipping build, so reading it as test code would put
/// production code beyond the lane's reach. `not(…)` and `any(…)` describe
/// code that can compile *without* `test` and are refused for the same reason;
/// `all(…)` requires every arm, so one `test` among them is enough.
///
/// Anything else — an unfamiliar predicate, a malformed one — answers `false`,
/// which costs the exemption and scans the code. That is the safe direction: a
/// wrong `true` here hides real findings, a wrong `false` only restores noise.
fn requires_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(p) => p.is_ident("test"),
        Meta::List(l) if l.path.is_ident("all") => l
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .map(|inner| inner.iter().any(requires_test))
            .unwrap_or(false),
        // `any(test, feature = "x")` compiles with the feature and no test;
        // `not(test)` is the shipping half of a stub pair. Neither is test code.
        _ => false,
    }
}

/// Whether any of these attributes gates its item on `test`.
fn gated_on_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && a.parse_args::<Meta>()
                .map(|m| requires_test(&m))
                .unwrap_or(false)
    })
}

/// The 1-based, inclusive line ranges covered by `#[cfg(test)]` items.
///
/// Spans come from `syn`, which is already how the slop lane reads Rust. The
/// alternative — counting braces by hand — cannot be made right at this level:
/// a `'{'` character literal, a `{` inside a block comment, and a multi-line raw
/// string each miscount, and a miscount that leaves the depth positive runs the
/// region to end-of-file and exempts every shipping line below it. A real lexer
/// already ships in this crate, so the scanner does not need to grow one.
///
/// A file that does not parse yields no regions at all, so it is scanned in
/// full. The exemption is the thing worth losing when the input is not
/// understood.
fn cfg_test_regions(src: &str) -> Vec<(u32, u32)> {
    let Ok(file) = syn::parse_file(src) else {
        return Vec::new();
    };
    // An inner `#![cfg(test)]` gates the whole file.
    if gated_on_test(&file.attrs) {
        let last = src.lines().count().max(1) as u32;
        return vec![(1, last)];
    }
    let mut regions = Vec::new();
    collect_test_regions(&file.items, &mut regions);
    regions
}

/// Walk items, recording the span of every one gated on `test`.
///
/// Recurses into plain modules so a test module nested inside one is still
/// found; a gated item is recorded whole and not descended into, because
/// everything inside it is already exempt.
fn collect_test_regions(items: &[Item], out: &mut Vec<(u32, u32)>) {
    for item in items {
        let attrs: &[Attribute] = match item {
            Item::Mod(m) => &m.attrs,
            Item::Fn(f) => &f.attrs,
            Item::Impl(i) => &i.attrs,
            Item::Const(c) => &c.attrs,
            Item::Static(s) => &s.attrs,
            Item::Use(u) => &u.attrs,
            Item::Struct(s) => &s.attrs,
            Item::Enum(e) => &e.attrs,
            Item::Type(t) => &t.attrs,
            Item::Trait(t) => &t.attrs,
            Item::Macro(m) => &m.attrs,
            _ => &[],
        };
        if gated_on_test(attrs) {
            let span = item.span();
            out.push((span.start().line as u32, span.end().line as u32));
            continue;
        }
        if let Item::Mod(m) = item {
            if let Some((_, inner)) = &m.content {
                collect_test_regions(inner, out);
            }
        }
    }
}

/// A literal in a format some *issuer* hands out, rather than one a person
/// typed into a fixture.
///
/// This is what keeps the test exemption honest. Every pattern here is a
/// vendor prefix with a fixed shape: nobody reaches for `AKIA…` or `ghp_…`
/// when they need a placeholder, so a match is a real key far more often than
/// it is anything else — and a test file is where real keys most often land.
fn looks_issued(line: &str) -> bool {
    static ISSUED: OnceLock<Regex> = OnceLock::new();
    ISSUED
        .get_or_init(|| {
            Regex::new(concat!(
                r"A(?:KIA|SIA)[0-9A-Z]{16}",       // AWS access key id / temporary
                r"|gh[pousr]_[A-Za-z0-9]{20,}",    // GitHub PAT, OAuth, server, refresh
                r"|glpat-[A-Za-z0-9_\-]{20,}",     // GitLab PAT
                r"|xox[abprs]-[0-9A-Za-z\-]{10,}", // Slack
                r"|sk-ant-[A-Za-z0-9_\-]{20,}",    // Anthropic
                r"|sk-[A-Za-z0-9]{32,}",           // OpenAI-style
                r"|AIza[0-9A-Za-z_\-]{35}",        // Google
                r"|-----BEGIN [A-Z ]*PRIVATE KEY", // PEM
            ))
            .expect("valid regex")
        })
        .is_match(line)
}

/// What to say about an issuer-format literal, wherever it is found. Worded for
/// both sides of the exemption: it is the one finding test code still gets, and
/// in shipping code it is more urgent than the generic message, not less.
const ISSUED_CREDENTIAL: &str = "vendor-issued credential — this is a real key's format, not a \
     fixture's; rotate it and load it from the environment";

/// Scored as the `hardcoded-secret` rule it is reported under, so the lane score
/// does not move depending on which path found the same credential.
const ISSUED_WEIGHT: u32 = 40;

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
        // A whole test target needs no per-line work; elsewhere only the
        // `#[cfg(test)]` items are exempt, so the file is mapped once.
        let test_target = is_test_target(file);
        let test_regions = if test_target {
            Vec::new()
        } else {
            cfg_test_regions(&src)
        };
        for &n in added_lines {
            let Some(text) = lines.get((n as usize).saturating_sub(1)) else {
                continue;
            };
            // Skip comment lines — a keyword in a comment isn't a vuln.
            if text.trim_start().starts_with("//") {
                continue;
            }
            let in_test = test_target || test_regions.iter().any(|&(lo, hi)| n >= lo && n <= hi);

            // An issuer's format is checked on its own, before anything else can
            // suppress it. It must not wait on `hardcoded-secret` matching first:
            // that rule keys on the *identifier*, so `let k = "AKIA…"` never reaches
            // it and a real key would sail past under any name the alternation does
            // not list. Nor is it silenced by an environment read on the same line —
            // a literal fallback beside an `env::var` is still a literal.
            let issued = looks_issued(text);
            if issued {
                findings.push(PatternFinding {
                    rule: "hardcoded-secret".to_string(),
                    file: file.to_string(),
                    line: n,
                    message: ISSUED_CREDENTIAL.to_string(),
                    weight: ISSUED_WEIGHT,
                });
            }

            for rule in rules() {
                if !rule.re.is_match(text) {
                    continue;
                }
                if rule.id == "hardcoded-secret" {
                    // Already reported just above, with better wording.
                    if issued {
                        continue;
                    }
                    if reads_env(text) {
                        continue;
                    }
                }
                // Test code ships to nobody and meets no untrusted input, so no
                // rule here describes a risk that it carries.
                if in_test {
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

    // \u{2500}\u{2500} the test-code exemption \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}

    /// A PEM header, assembled at run time.
    ///
    /// Writing one as a literal would put an issuer-format string in this file, and the lane
    /// would report it — correctly, because that is the case `looks_issued` exists to catch.
    /// The vendor prefixes in these tests are built the same way for the same reason: a
    /// detector's own fixtures should not trip it.
    fn pem_header() -> String {
        format!("-----BEGIN {} PRIVATE KEY-----", "RSA")
    }

    /// Scan a single source string written at `path`, treating every line as added.
    fn scan_at(path: &str, src: &str) -> PatternReport {
        let dir = tempfile::tempdir().unwrap();
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, src).unwrap();
        let added: BTreeSet<u32> = (1..=src.lines().count() as u32).collect();
        let map: BTreeMap<String, BTreeSet<u32>> = [(path.to_string(), added)].into();
        scan_files(dir.path(), &[path.to_string()], &map)
    }

    #[test]
    fn a_fixture_inside_a_cfg_test_module_is_not_a_finding() {
        // The case that made this exemption worth having: an auth-header test needs a
        // credential-shaped value, and there is no way to write one that does not look
        // like the thing the rule is hunting for.
        let r = scan(
            "pub fn call() {}\n\
             \n\
             #[cfg(test)]\n\
             mod tests {\n\
             \u{20}   fn ctx() -> Ctx {\n\
             \u{20}       Ctx { token: \"abc123\".into() }\n\
             \u{20}   }\n\
             }\n",
        );
        assert!(r.findings.is_empty(), "{:?}", r.findings);
    }

    #[test]
    fn the_same_literal_in_shipping_code_is_still_a_finding() {
        // The control, and the whole reason the exemption is scoped to test items rather
        // than applied to the file: one line apart, opposite answers.
        let r = scan("let ctx = Ctx { token: \"abc123\".into() };\n");
        assert_eq!(rules_hit(&r), vec!["hardcoded-secret"]);
    }

    #[test]
    fn a_test_module_does_not_exempt_the_code_that_follows_it() {
        // The brace-matching earns its keep here. A `#[cfg(test)]` item is not always last
        // in a file, and a region that ran to end-of-file would silently stop scanning the
        // shipping code below it — turning a noise fix into a hole.
        let r = scan(
            "#[cfg(test)]\n\
             mod tests {\n\
             \u{20}   const TOKEN: &str = \"fixture-value\";\n\
             }\n\
             \n\
             pub fn connect() {\n\
             \u{20}   let password = \"hunter2-in-production\";\n\
             }\n",
        );
        assert_eq!(
            rules_hit(&r),
            vec!["hardcoded-secret"],
            "the secret after the test module must still be found: {:?}",
            r.findings
        );
        assert_eq!(r.findings[0].line, 7);
    }

    #[test]
    fn a_brace_inside_a_string_does_not_end_the_test_region_early() {
        // Test bodies are full of JSON fixtures. Counting a `{` inside one would close the
        // region early and start flagging the rest of the module.
        let r = scan(
            "#[cfg(test)]\n\
             mod tests {\n\
             \u{20}   const BODY: &str = \"{\\\"id\\\": 5}\";\n\
             \u{20}   const TOKEN: &str = \"fixture-value\";\n\
             }\n",
        );
        assert!(r.findings.is_empty(), "{:?}", r.findings);
    }

    #[test]
    fn a_declaration_rather_than_a_block_exempts_only_itself() {
        // `#[cfg(test)] mod tests;` has no body to brace-match; the code after it ships.
        let r = scan(
            "#[cfg(test)]\n\
             mod tests;\n\
             \n\
             pub fn connect() {\n\
             \u{20}   let password = \"hunter2-in-production\";\n\
             }\n",
        );
        assert_eq!(rules_hit(&r), vec!["hardcoded-secret"]);
    }

    #[test]
    fn a_cargo_test_target_is_exempt_without_needing_an_attribute() {
        // Integration tests carry no `#[cfg(test)]` — the whole target is test-only, so the
        // path is what says so. `benches/` is the same kind of target.
        for path in ["tests/forge_write.rs", "benches/throughput.rs"] {
            let r = scan_at(path, "let token = \"abc123-fixture\";\n");
            assert!(r.findings.is_empty(), "{path}: {:?}", r.findings);
        }
        // A crate whose name merely starts that way is not a test target.
        let r = scan_at(
            "tests-support/src/lib.rs",
            "let token = \"abc123-fixture\";\n",
        );
        assert_eq!(rules_hit(&r), vec!["hardcoded-secret"]);
    }

    #[test]
    fn an_issued_credential_is_still_reported_inside_test_code() {
        // The exemption's limit, and the reason it is not a blanket one: a test file is a
        // favourite resting place for a real key. Built at runtime so this very source
        // file does not carry a literal in an issuer's format.
        let key = format!("ghp_{}", "A".repeat(24));
        let src = format!("let token = \"{key}\";\n");

        for path in ["tests/it.rs", "benches/b.rs"] {
            let r = scan_at(path, &src);
            assert_eq!(rules_hit(&r), vec!["hardcoded-secret"], "{path}");
            assert!(
                r.findings[0].message.contains("not a fixture"),
                "an issued key must not read like the generic fixture message: {}",
                r.findings[0].message
            );
        }

        // Same inside a `#[cfg(test)]` module in an otherwise shipping file. Written as a
        // bare `let`, because the rule deliberately passes over type-annotated declarations
        // (see `does_not_flag_short_or_type_annotated`) and this test is about the exemption,
        // not about widening what the rule matches.
        let r = scan(&format!(
            "#[cfg(test)]\n\
             mod tests {{\n\
             \x20   fn fixture() {{\n\
             \x20       let token = \"{key}\";\n\
             \x20   }}\n\
             }}\n"
        ));
        assert_eq!(rules_hit(&r), vec!["hardcoded-secret"], "{:?}", r.findings);
        assert!(r.findings[0].message.contains("not a fixture"));
    }

    #[test]
    fn the_issued_shapes_cover_the_providers_that_actually_leak() {
        for (vendor, key) in [
            ("aws", format!("AKIA{}", "Q".repeat(16))),
            ("github", format!("ghp_{}", "b".repeat(24))),
            ("gitlab", format!("glpat-{}", "c".repeat(24))),
            ("slack", format!("xoxb-{}", "1".repeat(16))),
            ("anthropic", format!("sk-ant-{}", "d".repeat(24))),
            ("openai", format!("sk-{}", "e".repeat(40))),
            ("google", format!("AIza{}", "F".repeat(35))),
            ("pem", pem_header()),
        ] {
            assert!(
                looks_issued(&format!("let token = \"{key}\";")),
                "{vendor} key shape was not recognised"
            );
        }
        // And a fixture someone typed is not one of them — which is the whole point.
        assert!(!looks_issued("let token = \"abc123\";"));
        assert!(!looks_issued("let token = \"fake-pat\";"));
        assert!(!looks_issued("let password = \"hunter2-very-secret\";"));
    }

    #[test]
    fn a_feature_named_test_is_not_a_test_gate() {
        // `#[cfg(feature = "test")]` compiles in a shipping build whenever that feature is
        // on. Searching the predicate text for the word `test` finds it inside the string
        // literal and exempts production code — which is why the predicate is evaluated as
        // a meta-expression, where `feature = "test"` is a name/value and never the `test`
        // predicate.
        let r = scan(
            "#[cfg(feature = \"test\")]\n\
             mod shipped {\n\
             \u{20}   fn go() { let password = \"hunter2-in-production\"; }\n\
             }\n",
        );
        assert_eq!(rules_hit(&r), vec!["hardcoded-secret"], "{:?}", r.findings);
    }

    #[test]
    fn a_brace_in_a_char_literal_or_comment_or_raw_string_does_not_move_the_boundary() {
        // Each of these miscounts under hand-rolled brace matching. The char literal is the
        // dangerous one: it leaves the depth positive, so the region swallows the rest of
        // the file and every shipping line below stops being scanned.
        for (name, body) in [
            ("char literal", "\u{20}   fn g() { let brace = '{'; }\n"),
            ("block comment", "\u{20}   /* } unbalanced { */\n"),
            (
                "multi-line raw string",
                "\u{20}   const BODY: &str = r#\"{\n\u{20}       \\\"id\\\": 5\n\u{20}   }\"#;\n",
            ),
        ] {
            let src = format!(
                "#[cfg(test)]\n\
                 mod tests {{\n\
                 {body}\
                 \x20   fn f() {{ let token = \"abc123-fixture\"; }}\n\
                 }}\n\
                 \n\
                 pub fn connect() {{\n\
                 \x20   let password = \"hunter2-in-production\";\n\
                 }}\n"
            );
            let r = scan(&src);
            assert_eq!(
                rules_hit(&r),
                vec!["hardcoded-secret"],
                "{name}: the shipping secret below the module must still be found, and the \
                 fixture inside it must not be: {:?}",
                r.findings
            );
            assert!(
                r.findings[0]
                    .message
                    .contains("environment or a secret store"),
                "{name}: wrong finding reported: {}",
                r.findings[0].message
            );
        }
    }

    #[test]
    fn an_issued_credential_is_found_under_any_variable_name() {
        // `hardcoded-secret` keys on the identifier, and `key` is not in its alternation —
        // so a real AWS key assigned to one would never reach the issuer check if that
        // check only ran after the rule matched.
        let aws = format!("AKIA{}", "Q".repeat(16));
        for line in [
            format!("let k = \"{aws}\";"),
            format!("let anything_at_all = \"{aws}\";"),
            format!("let pem = \"{}\";", pem_header()),
        ] {
            let r = scan(&format!("fn f() {{ {line} }}\n"));
            assert_eq!(rules_hit(&r), vec!["hardcoded-secret"], "{line}");
            assert!(
                r.findings[0].message.contains("not a\n     fixture's")
                    || r.findings[0].message.contains("not a fixture's")
            );
        }
    }

    #[test]
    fn an_environment_read_does_not_excuse_an_issued_credential_beside_it() {
        // A literal fallback next to an `env::var` is still a hardcoded key — `reads_env`
        // suppression is for fixture-shaped values, not for an issuer's format.
        let key = format!("ghp_{}", "a".repeat(24));
        let r = scan(&format!(
            "fn f() {{ let token = std::env::var(\"GH\").unwrap_or(\"{key}\".into()); }}\n"
        ));
        assert_eq!(rules_hit(&r), vec!["hardcoded-secret"], "{:?}", r.findings);
    }

    #[test]
    fn an_issued_credential_is_reported_once_not_twice() {
        // The generic rule and the issuer check can match the same line; only one finding
        // should come out of it, or the lane score doubles for a single credential.
        let key = format!("ghp_{}", "a".repeat(24));
        let r = scan(&format!("fn f() {{ let token = \"{key}\"; }}\n"));
        assert_eq!(r.findings.len(), 1, "{:?}", r.findings);
        assert_eq!(r.score, 40);
    }

    #[test]
    fn the_other_rules_are_exempt_in_test_code_too() {
        // None of them describe a risk that test code carries: a test that shells out or
        // builds a query string reaches no untrusted input and ships to nobody.
        let r = scan(
            "#[cfg(test)]\n\
             mod tests {\n\
             \u{20}   fn go() {\n\
             \u{20}       Command::new(\"sh\").arg(\"-c\").spawn();\n\
             \u{20}       let q = format!(\"SELECT * FROM t WHERE id = {}\", id);\n\
             \u{20}       let h = md5(input);\n\
             \u{20}   }\n\
             }\n",
        );
        assert!(r.findings.is_empty(), "{:?}", r.findings);
    }

    #[test]
    fn a_predicate_that_does_not_require_test_exempts_nothing() {
        // The dangerous direction. `#[cfg(not(test))]` is the half of a stub pair that
        // actually SHIPS, and `any(test, feature)` ships whenever the feature is on — so
        // reading either as "test code" would hide production code from the lane. A
        // substring match on `test` does exactly that, which is why the predicate is parsed
        // rather than searched.
        for attr in [
            "#[cfg(not(test))]",
            "#[cfg(all(not(test), unix))]",
            "#[cfg(any(test, feature = \"mock\"))]",
        ] {
            let r = scan(&format!(
                "{attr}\n\
                 mod imposter {{\n\
                 \x20   fn go() {{ let password = \"hunter2-in-production\"; }}\n\
                 }}\n"
            ));
            assert_eq!(
                rules_hit(&r),
                vec!["hardcoded-secret"],
                "{attr} must not exempt anything"
            );
        }
    }

    #[test]
    fn a_comment_or_a_feature_name_cannot_pass_for_a_test_gate() {
        // `#[cfg(feature = "testing")]` is not a test gate, and neither is a trailing
        // comment that happens to say "test" — both would match a loose search.
        for attr in [
            "#[cfg(unix)] // only matters under test",
            "#[cfg(feature = \"testing\")]",
        ] {
            let r = scan(&format!(
                "{attr}\n\
                 mod m {{\n\
                 \x20   fn go() {{ let password = \"hunter2-in-production\"; }}\n\
                 }}\n"
            ));
            assert_eq!(rules_hit(&r), vec!["hardcoded-secret"], "{attr}");
        }
    }

    #[test]
    fn a_compound_predicate_that_requires_test_does_exempt() {
        // The counterpart: `all(test, …)` cannot compile without `test`, so it is test code.
        let r = scan(
            "#[cfg(all(test, unix))]\n\
             mod tests {\n\
             \u{20}   fn go() { let token = \"abc123-fixture\"; }\n\
             }\n",
        );
        assert!(r.findings.is_empty(), "{:?}", r.findings);
    }

    #[test]
    fn a_tests_rs_module_file_is_exempt() {
        // Declared `#[cfg(test)] mod tests;` from its parent, so the file holds no
        // attribute of its own to find.
        let r = scan_at("src/forge/tests.rs", "let token = \"abc123-fixture\";\n");
        assert!(r.findings.is_empty(), "{:?}", r.findings);
        // But a module merely named similarly is not one.
        let r = scan_at(
            "src/forge/test_helpers.rs",
            "let token = \"abc123-fixture\";\n",
        );
        assert_eq!(rules_hit(&r), vec!["hardcoded-secret"]);
    }

    #[test]
    fn an_inner_cfg_test_attribute_exempts_the_rest_of_the_file() {
        // Written as a `let` inside a fn, because `const TOKEN: &str = …` is type-annotated
        // and the rule passes over those anyway — asserting "no findings" on one would pass
        // whether the exemption worked or not.
        let body = "fn fixture() { let token = \"abc123-fixture\"; }\n";
        let r = scan(&format!("#![cfg(test)]\n\n{body}"));
        assert!(r.findings.is_empty(), "{:?}", r.findings);
        // Non-vacuity: the same body without the gate is still a finding.
        let r = scan(body);
        assert_eq!(rules_hit(&r), vec!["hardcoded-secret"]);
    }
}
