// SPDX-License-Identifier: Apache-2.0
//! Weakened-test lane: tests a change deletes, and tests that check less after
//! it than before.
//!
//! Mutation testing cannot see this. `cargo-mutants --in-diff` mutates changed
//! *code*, so a PR that only deletes a test, or trims its assertions, touches
//! nothing mutable and sails through — the exact shape of an agent "fixing" a
//! red suite. This lane compares each changed Rust file's tests at base and at
//! head (see [`crate::diff::RustVersions`]):
//!
//! - **`test-removed`** — fewer tests carry a name at head than at base, across
//!   the changed files. Counted rather than looked up, because names repeat: a
//!   `works` or `roundtrip` per module is common, and one surviving copy must
//!   not hide another's deletion. A test that moved (renamed file, split module)
//!   leaves its name's count where it was, so it is not reported: fewer false
//!   alarms over catching a rename-plus-delete.
//! - **`assertions-reduced`** — a test present on both sides whose assertion
//!   count fell. Counted: `assert*!` macros and `#[should_panic]`, the things
//!   that make a test *check* something, outside comments — commenting out the
//!   failing assertion is the commonest way to quiet a red test. `.unwrap()` and
//!   `?` are left out on purpose; they come and go with refactors and would make
//!   the count noise.
//!
//! Advisory like the other pattern lanes; `--block-on-pattern weakened-tests`
//! (or a rule id) makes it block.

use std::collections::HashMap;

use crate::diff::RustVersions;
use crate::pattern::{PatternFinding, PatternReport};
use crate::zero_assertion::{tests_in, TestFn};

/// Compare base and head tests across every changed Rust file.
pub fn scan(versions: &[RustVersions]) -> PatternReport {
    let sides: Vec<(Vec<TestFn>, Vec<TestFn>)> = versions
        .iter()
        .map(|v| {
            let head = v.head.as_deref().map(tests_in).unwrap_or_default();
            (tests_in(&v.base), head)
        })
        .collect();

    // How many tests with each name the change removed outright: the base count
    // across every changed file, less the head count. A move keeps the count.
    let mut removed_by_change: HashMap<&str, usize> = HashMap::new();
    for (base, _) in &sides {
        for t in base {
            *removed_by_change.entry(t.name.as_str()).or_default() += 1;
        }
    }
    for (_, head) in &sides {
        for t in head {
            if let Some(n) = removed_by_change.get_mut(t.name.as_str()) {
                *n = n.saturating_sub(1);
            }
        }
    }

    let mut findings = Vec::new();
    for (v, (base, head)) in versions.iter().zip(&sides) {
        let head = by_name(head);
        for (name, at_base) in by_name(base) {
            let at_head = head
                .iter()
                .find(|(n, _)| *n == name)
                .map_or(&[][..], |(_, ts)| ts.as_slice());
            if at_head.len() == at_base.len() {
                // The same tests, in source order: compare each one's checks.
                for (b, h) in at_base.iter().zip(at_head) {
                    let before = checks(&b.body, b.should_panic);
                    let after = checks(&h.body, h.should_panic);
                    if after < before {
                        findings.push(PatternFinding {
                            rule: "assertions-reduced".into(),
                            file: v.path.clone(),
                            line: h.line,
                            message: format!(
                                "test `{name}` checks less: {before} → {after} assertion(s)"
                            ),
                            weight: 10,
                        });
                    }
                }
                continue;
            }
            if at_head.len() > at_base.len() {
                continue; // gained tests of this name here; lost none
            }
            // This file lost some; report as many as the change lost overall
            // (the rest moved to another changed file).
            let lost_here = at_base.len() - at_head.len();
            let unclaimed = removed_by_change.get_mut(name).expect("counted above");
            let removed = lost_here.min(*unclaimed);
            *unclaimed -= removed;
            // The test is gone, so there is no head line to point at: file-level,
            // with the base line in the message where it is known which one went.
            let removal = |message: String| PatternFinding {
                rule: "test-removed".into(),
                file: v.path.clone(),
                line: 0,
                message,
                weight: 20,
            };
            if removed == at_base.len() {
                // Every test of that name here went, so each is known.
                for t in at_base {
                    findings.push(removal(if v.head.is_some() {
                        format!("test `{name}` was removed (line {} at base)", t.line)
                    } else {
                        format!("test `{name}` was removed with its file")
                    }));
                }
            } else {
                // Several share the name and only some went: which ones, a name
                // cannot tell. Say how many.
                for _ in 0..removed {
                    findings.push(removal(format!(
                        "a test named `{name}` was removed ({} at base, {} at head)",
                        at_base.len(),
                        at_head.len()
                    )));
                }
            }
        }
    }
    PatternReport::from_findings(findings)
}

/// `tests` grouped by name, in order of first appearance, each group in source
/// order — so findings come out in the order the tests are written.
fn by_name(tests: &[TestFn]) -> Vec<(&str, Vec<&TestFn>)> {
    let mut groups: Vec<(&str, Vec<&TestFn>)> = Vec::new();
    for t in tests {
        match groups.iter_mut().find(|(n, _)| *n == t.name) {
            Some((_, group)) => group.push(t),
            None => groups.push((&t.name, vec![t])),
        }
    }
    groups
}

/// Assertions in a test body: macro calls named `assert*!` or `debug_assert*!`
/// (whole identifiers, so `reassert!` or a variable named `asserted` do not
/// count) plus one for `#[should_panic]`, which is the test's check. Only code
/// counts: an assertion commented out has stopped checking anything.
fn checks(body: &str, should_panic: bool) -> usize {
    let body = &strip_comments(body);
    let bytes = body.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut n = usize::from(should_panic);
    let mut i = 0;
    while i < bytes.len() {
        if !is_ident(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_ident(bytes[i]) {
            i += 1;
        }
        let name = &body[start..i];
        if bytes.get(i) == Some(&b'!')
            && (name.starts_with("assert") || name.starts_with("debug_assert"))
        {
            n += 1;
        }
    }
    n
}

/// `src` without its `//` and `/* */` comments (block comments nest in Rust).
/// String literals, and the char literal `'"'`, are kept whole, so a `//` or a
/// quote inside one does not throw the rest of the line out with it.
fn strip_comments(src: &str) -> String {
    let s: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < s.len() {
        match (s[i], s.get(i + 1).copied()) {
            ('/', Some('/')) => {
                while i < s.len() && s[i] != '\n' {
                    i += 1;
                }
            }
            ('/', Some('*')) => {
                let mut depth = 0;
                while i < s.len() {
                    match (s[i], s.get(i + 1).copied()) {
                        ('/', Some('*')) => {
                            depth += 1;
                            i += 2;
                        }
                        ('*', Some('/')) => {
                            depth -= 1;
                            i += 2;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => i += 1,
                    }
                }
                out.push(' ');
            }
            ('\'', Some('"')) if s.get(i + 2) == Some(&'\'') => {
                out.extend(&s[i..i + 3]);
                i += 3;
            }
            ('"', _) => {
                out.push('"');
                i += 1;
                while i < s.len() {
                    let c = s[i];
                    out.push(c);
                    i += 1;
                    if c == '\\' {
                        if let Some(&escaped) = s.get(i) {
                            out.push(escaped);
                            i += 1;
                        }
                    } else if c == '"' {
                        break;
                    }
                }
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(path: &str, base: &str, head: Option<&str>) -> RustVersions {
        RustVersions {
            path: path.into(),
            base: base.into(),
            head: head.map(Into::into),
        }
    }

    const BASE: &str = "\
#[cfg(test)]
mod tests {
    #[test]
    fn adds() {
        assert_eq!(super::add(2, 3), 5);
        assert_eq!(super::add(-1, 1), 0);
    }

    #[test]
    fn subtracts() {
        assert!(super::sub(3, 2) == 1);
    }
}
";

    fn rules(r: &PatternReport) -> Vec<(&str, &str)> {
        r.findings
            .iter()
            .map(|f| (f.rule.as_str(), f.message.as_str()))
            .collect()
    }

    #[test]
    fn an_unchanged_suite_is_clean() {
        assert!(scan(&[v("src/lib.rs", BASE, Some(BASE))])
            .findings
            .is_empty());
    }

    #[test]
    fn a_deleted_test_is_reported_file_level() {
        let head = BASE.replace(
            "    #[test]\n    fn subtracts() {\n        assert!(super::sub(3, 2) == 1);\n    }\n",
            "",
        );
        let r = scan(&[v("src/lib.rs", BASE, Some(&head))]);
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].rule, "test-removed");
        assert_eq!(r.findings[0].line, 0);
        assert!(r.findings[0].message.contains("`subtracts`"));
    }

    #[test]
    fn a_dropped_assertion_is_reported_on_the_test() {
        let head = BASE.replace("        assert_eq!(super::add(-1, 1), 0);\n", "");
        let r = scan(&[v("src/lib.rs", BASE, Some(&head))]);
        assert_eq!(
            rules(&r),
            [(
                "assertions-reduced",
                "test `adds` checks less: 2 → 1 assertion(s)"
            )]
        );
        assert_eq!(r.findings[0].line, 4);
    }

    #[test]
    fn a_deleted_test_file_reports_every_test_in_it() {
        let r = scan(&[v("tests/math.rs", BASE, None)]);
        assert_eq!(r.findings.len(), 2);
        assert!(r.findings.iter().all(|f| f.rule == "test-removed"));
        assert!(r.findings[0].message.contains("with its file"));
    }

    #[test]
    fn a_test_moved_to_another_file_is_not_a_removal() {
        // The file was split: `subtracts` now lives in a new file. Same name,
        // somewhere in the change — not a deletion.
        let head = BASE.replace(
            "    #[test]\n    fn subtracts() {\n        assert!(super::sub(3, 2) == 1);\n    }\n",
            "",
        );
        let moved = "#[test]\nfn subtracts() {\n    assert!(crate::sub(3, 2) == 1);\n}\n";
        let r = scan(&[
            v("src/lib.rs", BASE, Some(&head)),
            v("tests/sub.rs", "", Some(moved)),
        ]);
        assert!(r.findings.is_empty(), "{:?}", rules(&r));
    }

    #[test]
    fn more_or_reworded_assertions_are_fine() {
        let head = BASE.replace(
            "        assert!(super::sub(3, 2) == 1);\n",
            "        assert_eq!(super::sub(3, 2), 1);\n        assert_eq!(super::sub(2, 2), 0);\n",
        );
        assert!(scan(&[v("src/lib.rs", BASE, Some(&head))])
            .findings
            .is_empty());
    }

    #[test]
    fn only_whole_assert_macros_count() {
        assert_eq!(
            checks("assert!(a); assert_eq!(a, b); debug_assert!(c);", false),
            3
        );
        assert_eq!(
            checks("let asserted = reassert!(x); // assert things", false),
            0
        );
        assert_eq!(checks("", true), 1, "#[should_panic] is the test's check");
    }

    /// Two modules, each with a test named `works` — one per module is common.
    const TWO_WORKS: &str = "\
mod parse {
    #[test]
    fn works() {
        assert!(super::parse(\"1\").is_ok());
        assert!(super::parse(\"x\").is_err());
    }
}

mod render {
    #[test]
    fn works() {
        assert_eq!(super::render(1), \"1\");
        assert_eq!(super::render(-1), \"-1\");
    }
}
";

    fn without_render_works(src: &str) -> String {
        let start = src.find("mod render").unwrap();
        src[..start].to_string()
    }

    #[test]
    fn a_surviving_same_named_test_does_not_hide_a_removal() {
        let r = scan(&[v(
            "src/lib.rs",
            TWO_WORKS,
            Some(&without_render_works(TWO_WORKS)),
        )]);
        assert_eq!(
            rules(&r),
            [(
                "test-removed",
                "a test named `works` was removed (2 at base, 1 at head)"
            )]
        );
    }

    #[test]
    fn a_same_named_test_in_another_changed_file_does_not_hide_a_removal() {
        // Before, any `roundtrip` left anywhere in the change read as "moved".
        let a = "#[test]\nfn roundtrip() {\n    assert!(super::a());\n}\n";
        let b = "#[test]\nfn roundtrip() {\n    assert!(super::b());\n}\n";
        let b_head = format!("{b}#[test]\nfn another() {{\n    assert!(true);\n}}\n");
        let r = scan(&[v("src/a.rs", a, Some("")), v("src/b.rs", b, Some(&b_head))]);
        assert_eq!(r.findings.len(), 1, "{:?}", rules(&r));
        assert_eq!(r.findings[0].rule, "test-removed");
        assert_eq!(r.findings[0].file, "src/a.rs");
    }

    #[test]
    fn a_test_moved_beside_one_of_the_same_name_is_not_a_removal() {
        let a = "#[test]\nfn works() {\n    assert!(super::a());\n}\n";
        let r = scan(&[
            v("src/a.rs", a, Some("")),
            v(
                "src/b.rs",
                TWO_WORKS,
                Some(&format!("{TWO_WORKS}mod a {{\n{a}}}\n")),
            ),
        ]);
        assert!(r.findings.is_empty(), "{:?}", rules(&r));
    }

    #[test]
    fn same_named_tests_are_compared_one_to_one() {
        let head = TWO_WORKS.replace("        assert_eq!(super::render(-1), \"-1\");\n", "");
        let r = scan(&[v("src/lib.rs", TWO_WORKS, Some(&head))]);
        assert_eq!(
            rules(&r),
            [(
                "assertions-reduced",
                "test `works` checks less: 2 → 1 assertion(s)"
            )]
        );
        assert_eq!(r.findings[0].line, 11, "the render module's `works`");
    }

    #[test]
    fn commenting_out_an_assertion_is_checking_less() {
        for commented in [
            "        // assert_eq!(super::add(-1, 1), 0);\n",
            "        /* assert_eq!(super::add(-1, 1), 0); */\n",
        ] {
            let head = BASE.replace("        assert_eq!(super::add(-1, 1), 0);\n", commented);
            let r = scan(&[v("src/lib.rs", BASE, Some(&head))]);
            assert_eq!(
                rules(&r),
                [(
                    "assertions-reduced",
                    "test `adds` checks less: 2 → 1 assertion(s)"
                )],
                "{commented}"
            );
        }
    }

    #[test]
    fn only_code_counts_and_string_literals_stay_whole() {
        assert_eq!(
            checks(
                "assert!(a); // assert!(b)\n/* assert!(c) /* nested */ assert!(d) */ assert!(e);",
                false
            ),
            2
        );
        assert_eq!(
            checks(r#"let s = "// not a comment"; assert!(ok);"#, false),
            1
        );
        // A `'"'` char literal must not open a string that swallows the comment.
        assert_eq!(checks("let q = '\"'; // assert!(x)\nassert!(y);", false), 1);
    }
}
