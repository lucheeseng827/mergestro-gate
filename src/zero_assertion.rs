// SPDX-License-Identifier: Apache-2.0
//! Free second signal: flag tests on the changed surface that assert nothing.
//!
//! A test that calls code but never checks the result still goes green — and
//! mutation testing can't catch what no test exercises meaningfully. This is a
//! cheap *static* pre-check, deliberately tuned for **few false positives**: a
//! test is flagged only when its body contains no assertion-like check at all
//! (`assert*!`, `panic!`, `?`, `.unwrap`/`.expect`, `#[should_panic]`, …).
//!
//! It's a line-oriented heuristic, not a parser — we keep the dependency
//! footprint small rather than pulling in `syn`. Brace matching is naive (it
//! doesn't track string/char literals), which at worst mis-scopes an exotic
//! body; the conservative marker set keeps that from producing false alarms.

use std::path::Path;

use crate::report::ZeroAssertionFinding;

/// Markers that count as "this test checks something". If a test body contains
/// none of these, and it isn't `#[should_panic]`, it's flagged.
const CHECK_MARKERS: &[&str] = &[
    "assert",
    "panic!",
    "unreachable!",
    "unimplemented!",
    "todo!",
    ".unwrap",
    ".expect",
    "?",
];

/// Scan each changed file on disk (head working tree) for assertion-free tests.
pub fn scan_files(repo: &Path, files: &[String]) -> Vec<ZeroAssertionFinding> {
    let mut out = Vec::new();
    for f in files {
        if let Ok(content) = std::fs::read_to_string(repo.join(f)) {
            out.extend(scan_source(f, &content));
        }
    }
    out
}

/// Scan a single source string for `#[test]` functions whose bodies contain no
/// assertion-like check.
pub fn scan_source(path: &str, content: &str) -> Vec<ZeroAssertionFinding> {
    let lines: Vec<&str> = content.lines().collect();
    let mut findings = Vec::new();
    let mut is_test = false;
    let mut should_panic = false;

    for (idx, raw) in lines.iter().enumerate() {
        let line = raw.trim_start();

        if line.is_empty() || line.starts_with("//") {
            continue;
        }

        if let Some(attr) = attribute_path(line) {
            if attr == "test" || attr.ends_with("::test") {
                is_test = true;
            } else if attr.starts_with("should_panic") {
                should_panic = true;
            }
            // Other attributes (#[ignore], #[cfg(...)], …) stack; don't reset.
            continue;
        }

        if is_test {
            if let Some(name) = function_name(line) {
                let body = body_after(&lines, idx);
                if !has_check(&body, should_panic) {
                    findings.push(ZeroAssertionFinding {
                        file: path.to_string(),
                        line: (idx + 1) as u32,
                        function: name,
                    });
                }
                is_test = false;
                should_panic = false;
                continue;
            }
        }

        // A meaningful non-attribute line that wasn't the test fn ends the
        // pending attribute group.
        is_test = false;
        should_panic = false;
    }

    findings
}

/// If `line` is an attribute, return its path (the bit before any `(`/`]`),
/// e.g. `#[tokio::test]` → `tokio::test`, `#[cfg(test)]` → `cfg`.
fn attribute_path(line: &str) -> Option<String> {
    let inner = line.strip_prefix("#[")?;
    let end = inner.find(['(', ']', ' ']).unwrap_or(inner.len());
    Some(inner[..end].trim().to_string())
}

/// Extract the function name from a line that defines one, e.g.
/// `    pub async fn foo() {` → `foo`. Returns `None` if not a fn definition.
fn function_name(line: &str) -> Option<String> {
    // Find the `fn` keyword as a whole word.
    let after_fn = line.split_whitespace().skip_while(|t| *t != "fn").nth(1)?;
    let name: String = after_fn
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Collect the text between the outermost braces of the item starting at
/// `start`, matching across lines. Naive (ignores string/char literals).
fn body_after(lines: &[&str], start: usize) -> String {
    let mut depth = 0i32;
    let mut started = false;
    let mut body = String::new();
    for line in &lines[start..] {
        for ch in line.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    started = true;
                    if depth == 1 {
                        continue; // don't include the opening brace
                    }
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return body;
                    }
                }
                _ => {}
            }
            if started && depth >= 1 {
                body.push(ch);
            }
        }
        if started {
            body.push('\n');
        }
    }
    body
}

/// Whether a test body contains any assertion-like check.
fn has_check(body: &str, should_panic: bool) -> bool {
    should_panic || CHECK_MARKERS.iter().any(|m| body.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_assertion_free_test() {
        let src = r#"
            #[test]
            fn theater() {
                let _ = is_adult(20);
                let _ = is_adult(10);
            }
        "#;
        let f = scan_source("src/lib.rs", src);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].function, "theater");
        assert_eq!(f[0].file, "src/lib.rs");
    }

    #[test]
    fn does_not_flag_test_with_assert() {
        let src = r#"
            #[test]
            fn real() {
                assert_eq!(is_adult(18), true);
            }
        "#;
        assert!(scan_source("x.rs", src).is_empty());
    }

    #[test]
    fn does_not_flag_should_panic() {
        let src = r#"
            #[test]
            #[should_panic]
            fn boom() {
                trigger();
            }
        "#;
        assert!(scan_source("x.rs", src).is_empty());
    }

    #[test]
    fn does_not_flag_unwrap_or_question_mark() {
        let unwrap = "#[test]\nfn t() {\n    do_it().unwrap();\n}\n";
        let question = "#[test]\nfn t() -> Result<(), E> {\n    do_it()?;\n    Ok(())\n}\n";
        assert!(scan_source("x.rs", unwrap).is_empty());
        assert!(scan_source("x.rs", question).is_empty());
    }

    #[test]
    fn cfg_test_module_is_not_treated_as_a_test() {
        let src = r#"
            #[cfg(test)]
            mod tests {
                fn helper() { do_nothing(); }
            }
        "#;
        assert!(scan_source("x.rs", src).is_empty());
    }

    #[test]
    fn handles_tokio_test_attribute() {
        let src = "#[tokio::test]\nasync fn t() {\n    let _ = call().await;\n}\n";
        let f = scan_source("x.rs", src);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].function, "t");
    }

    #[test]
    fn non_test_functions_are_ignored() {
        let src = "fn helper() {\n    something();\n}\n";
        assert!(scan_source("x.rs", src).is_empty());
    }
}
