// SPDX-License-Identifier: Apache-2.0
//! Pattern lane — **AI-slop signatures** (Track B, lane 1).
//!
//! Mutation testing answers "do the tests catch behaviour changes?" — it says
//! nothing about whether the *code itself* is bloated, low-information slop. This
//! lane adds that signal: a cheap, static, diff-scoped scan for structural
//! patterns typical of LLM-generated filler, producing a 0–100 **slop score**
//! that feeds the report and the telemetry (so the score can be trended — the
//! Track C KPI question: "is slop score a believable metric?").
//!
//! It is **advisory** — it never changes the verdict. It is also nearly free:
//! it parses the changed Rust to an AST (it does not build it), so it can run on
//! every PR even when mutation is suppressed.
//!
//! Detectors (conservative by design — better a missed slop than a false alarm):
//! - **redundant-wrapper** — a function whose whole body just forwards its
//!   arguments, unchanged and in order, to another function.
//! - **tautological-assert** — `assert!(true)` / `assert_eq!(x, x)` and their
//!   `debug_`/`_ne` kin: a test line that can never fail.
//! - **over-commented** — a changed block where comments dominate added code.
//!
//! Diff scope: AST findings are kept only when their line is in the diff's added
//! lines (parsed from the unified diff), so pre-existing code in a touched file
//! is never blamed on the PR.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use quote::ToTokens;
use syn::punctuated::Punctuated;
use syn::visit::Visit;
use syn::{Expr, Lit, Stmt, Token};

use crate::pattern::{PatternFinding, PatternReport};

const W_WRAPPER: u32 = 25;
const W_TAUTOLOGICAL: u32 = 20;
const W_OVER_COMMENT: u32 = 15;
/// Don't flag over-commenting on a trivially small added block.
const MIN_ADDED_CODE_FOR_COMMENT_RULE: usize = 6;

/// Scan the changed Rust files for slop signatures, scoped to `added` (the
/// diff's added line numbers, by file). Files with no tracked added lines, files
/// that can't be read, and files that don't parse are skipped — the lane is
/// best-effort and never fails the run.
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
        if let Ok(ast) = syn::parse_file(&src) {
            let mut v = SlopVisitor {
                file,
                added: added_lines,
                findings: &mut findings,
            };
            v.visit_file(&ast);
        }
        if let Some(f) = over_comment_finding(file, &src, added_lines) {
            findings.push(f);
        }
    }
    PatternReport::from_findings(findings)
}

/// Parse a unified diff into the set of **new-side** added line numbers per file.
/// Used to scope findings to what the PR actually added.
pub fn added_lines(unified: &str) -> BTreeMap<String, BTreeSet<u32>> {
    let mut map: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    let mut cur: Option<String> = None;
    let mut new_line: u32 = 0;
    let mut in_hunk = false;
    for line in unified.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            // New-file header: `+++ b/path` (or `+++ path`, or `/dev/null`).
            let path = rest
                .trim()
                .strip_prefix("b/")
                .unwrap_or_else(|| rest.trim());
            cur = if path == "/dev/null" {
                None
            } else {
                Some(path.to_string())
            };
            in_hunk = false;
            continue;
        }
        if line.starts_with("--- ") {
            continue;
        }
        if let Some(h) = line.strip_prefix("@@") {
            // `@@ -a,b +c,d @@` — take the new-side start `c`.
            new_line = parse_hunk_new_start(h).unwrap_or(new_line);
            in_hunk = true;
            continue;
        }
        if !in_hunk || cur.is_none() {
            continue;
        }
        // Within a hunk, classify by the first column.
        match line.as_bytes().first() {
            Some(b'+') => {
                if let Some(file) = &cur {
                    map.entry(file.clone()).or_default().insert(new_line);
                }
                new_line += 1;
            }
            Some(b'-') => { /* removed: no new-side advance */ }
            Some(b'\\') => { /* "\ No newline at end of file" */ }
            _ => new_line += 1, // context line
        }
    }
    map
}

/// From `@@ -a,b +c,d @@`, parse the new-side start `c`.
fn parse_hunk_new_start(h: &str) -> Option<u32> {
    let plus = h.split('+').nth(1)?;
    let num = plus.split(|c: char| c == ',' || c.is_whitespace()).next()?;
    num.parse().ok()
}

struct SlopVisitor<'a> {
    file: &'a str,
    added: &'a BTreeSet<u32>,
    findings: &'a mut Vec<PatternFinding>,
}

impl<'a> SlopVisitor<'a> {
    fn push(&mut self, rule: &str, line: u32, message: String, weight: u32) {
        if self.added.contains(&line) {
            self.findings.push(PatternFinding {
                rule: rule.to_string(),
                file: self.file.to_string(),
                line,
                message,
                weight,
            });
        }
    }
}

impl<'ast> Visit<'ast> for SlopVisitor<'_> {
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        if let Some(msg) = wrapper_message(&f.sig, &f.block) {
            self.push("redundant-wrapper", line_of(&f.sig.ident), msg, W_WRAPPER);
        }
        syn::visit::visit_item_fn(self, f);
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        if let Some(msg) = wrapper_message(&f.sig, &f.block) {
            self.push("redundant-wrapper", line_of(&f.sig.ident), msg, W_WRAPPER);
        }
        syn::visit::visit_impl_item_fn(self, f);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if let Some(msg) = tautological_assert_message(m) {
            let line = m
                .path
                .segments
                .last()
                .map(|s| line_of(&s.ident))
                .unwrap_or(0);
            self.push("tautological-assert", line, msg, W_TAUTOLOGICAL);
        }
        syn::visit::visit_macro(self, m);
    }
}

/// 1-based source line of an identifier (requires proc-macro2 span-locations).
fn line_of(ident: &syn::Ident) -> u32 {
    ident.span().start().line as u32
}

/// If `sig`/`block` is a function whose entire body forwards its parameters —
/// unchanged and in order — to another function, return a description. Skips
/// methods with a receiver, non-trivial parameter patterns, recursion, and
/// zero-argument aliases (those are often legitimate renames).
fn wrapper_message(sig: &syn::Signature, block: &syn::Block) -> Option<String> {
    let params = simple_param_idents(sig)?;
    if params.is_empty() {
        return None;
    }
    if block.stmts.len() != 1 {
        return None;
    }
    // The single statement must *return* a call (tail expression or `return`).
    let call = match &block.stmts[0] {
        Stmt::Expr(Expr::Call(c), None) => c,
        Stmt::Expr(Expr::Return(r), _) => match r.expr.as_deref() {
            Some(Expr::Call(c)) => c,
            _ => return None,
        },
        _ => return None,
    };
    // Args must be exactly the params, as plain identifiers, in order.
    if call.args.len() != params.len() {
        return None;
    }
    for (arg, param) in call.args.iter().zip(&params) {
        if expr_ident(arg)? != *param {
            return None;
        }
    }
    let callee = path_tail(&call.func)?;
    if sig.ident == callee {
        return None; // self-call, not a wrapper
    }
    Some(format!(
        "`{}` only forwards its arguments to `{}` — redundant wrapper",
        sig.ident, callee
    ))
}

/// Parameter identifiers when every parameter is a plain `name: T` binding and
/// there is no `self` receiver; otherwise `None` (can't match simply).
fn simple_param_idents(sig: &syn::Signature) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for input in &sig.inputs {
        match input {
            syn::FnArg::Receiver(_) => return None,
            syn::FnArg::Typed(pt) => match pt.pat.as_ref() {
                syn::Pat::Ident(pi) => out.push(pi.ident.to_string()),
                _ => return None,
            },
        }
    }
    Some(out)
}

/// A bare-identifier expression's name, else `None`.
fn expr_ident(e: &Expr) -> Option<String> {
    match e {
        Expr::Path(p) => p.path.get_ident().map(|i| i.to_string()),
        _ => None,
    }
}

/// Last path segment of a call target (the callee's name).
fn path_tail(func: &Expr) -> Option<String> {
    match func {
        Expr::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

/// If a macro invocation is an always-true assertion, describe it.
fn tautological_assert_message(m: &syn::Macro) -> Option<String> {
    let name = m.path.segments.last()?.ident.to_string();
    match name.as_str() {
        "assert" | "debug_assert" => {
            let args = m
                .parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated)
                .ok()?;
            let cond = args.first()?;
            if is_true_literal(cond) {
                Some(format!("`{name}!(true)` can never fail"))
            } else {
                None
            }
        }
        "assert_eq" | "assert_ne" | "debug_assert_eq" | "debug_assert_ne" => {
            let args = m
                .parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated)
                .ok()?;
            let mut it = args.iter();
            let a = it.next()?;
            let b = it.next()?;
            if is_plain_value_expr(a) && is_plain_value_expr(b) && tokens_eq(a, b) {
                let verb = if name.contains("_ne") {
                    "can never hold"
                } else {
                    "always holds"
                };
                Some(format!("`{name}!` compares a value to itself — {verb}"))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn is_true_literal(e: &Expr) -> bool {
    matches!(e, Expr::Lit(l) if matches!(&l.lit, Lit::Bool(b) if b.value))
}

/// Token-stream equality of two expressions (so `x` == `x`, `a.b` == `a.b`).
fn tokens_eq(a: &Expr, b: &Expr) -> bool {
    a.to_token_stream().to_string() == b.to_token_stream().to_string()
}

/// True when `e` is a plain value (literal or simple path) whose evaluation
/// cannot have side-effects. Used to guard the tautological-assert check:
/// `assert_eq!(next(), next())` has identical tokens but can fail at runtime.
fn is_plain_value_expr(e: &Expr) -> bool {
    matches!(e, Expr::Path(p) if p.qself.is_none()) || matches!(e, Expr::Lit(_))
}

/// Flag a file whose **added** lines are comment-dominated (a hallmark of
/// LLM filler), once there's enough added code to judge.
fn over_comment_finding(file: &str, src: &str, added: &BTreeSet<u32>) -> Option<PatternFinding> {
    let lines: Vec<&str> = src.lines().collect();
    let mut comment = 0usize;
    let mut code = 0usize;
    let mut first_comment_line = 0u32;
    for &n in added {
        let Some(text) = lines.get((n as usize).saturating_sub(1)) else {
            continue;
        };
        let t = text.trim_start();
        if t.is_empty() {
            continue;
        }
        if t.starts_with("//") {
            comment += 1;
            if first_comment_line == 0 {
                first_comment_line = n;
            }
        } else {
            code += 1;
        }
    }
    let total = comment + code;
    if code < MIN_ADDED_CODE_FOR_COMMENT_RULE || total == 0 {
        return None;
    }
    let ratio = comment as f64 / total as f64;
    if ratio <= 0.5 {
        return None;
    }
    Some(PatternFinding {
        rule: "over-commented".to_string(),
        file: file.to_string(),
        line: first_comment_line.max(1),
        message: format!(
            "{:.0}% of the added non-blank lines are comments",
            ratio * 100.0
        ),
        weight: W_OVER_COMMENT,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_lines(src: &str) -> BTreeSet<u32> {
        (1..=src.lines().count() as u32).collect()
    }

    fn scan_src(src: &str) -> PatternReport {
        // Scan a source string directly (every line considered "added").
        let mut findings = Vec::new();
        let added = all_lines(src);
        let ast = syn::parse_file(src).expect("parses");
        let mut v = SlopVisitor {
            file: "x.rs",
            added: &added,
            findings: &mut findings,
        };
        v.visit_file(&ast);
        if let Some(f) = over_comment_finding("x.rs", src, &added) {
            findings.push(f);
        }
        PatternReport::from_findings(findings)
    }

    #[test]
    fn flags_redundant_wrapper() {
        let r = scan_src("pub fn outer(a: u32, b: u32) -> u32 { inner(a, b) }\nfn inner(a: u32, b: u32) -> u32 { a + b }\n");
        assert!(r.findings.iter().any(|f| f.rule == "redundant-wrapper"));
    }

    #[test]
    fn ignores_non_forwarding_function() {
        // Reorders args → not a pure forward.
        let r = scan_src("fn outer(a: u32, b: u32) -> u32 { inner(b, a) }\n");
        assert!(!r.findings.iter().any(|f| f.rule == "redundant-wrapper"));
    }

    #[test]
    fn ignores_real_work_function() {
        let r = scan_src("fn outer(a: u32, b: u32) -> u32 { a * b + 1 }\n");
        assert!(r.findings.is_empty());
    }

    #[test]
    fn flags_tautological_asserts() {
        let r = scan_src("#[test]\nfn t() {\n  assert!(true);\n  assert_eq!(x, x);\n}\n");
        let rules: Vec<_> = r.findings.iter().map(|f| f.rule.as_str()).collect();
        assert_eq!(
            rules
                .iter()
                .filter(|r| **r == "tautological-assert")
                .count(),
            2
        );
    }

    #[test]
    fn does_not_flag_real_assert() {
        let r = scan_src("#[test]\nfn t() { assert_eq!(add(2, 2), 4); }\n");
        assert!(!r.findings.iter().any(|f| f.rule == "tautological-assert"));
    }

    #[test]
    fn does_not_flag_identical_call_exprs_as_tautological() {
        // assert_eq!(next(), next()) has matching tokens but is NOT a tautology —
        // the two calls may return different values.
        let r = scan_src("#[test]\nfn t() { assert_eq!(next(), next()); }\n");
        assert!(
            !r.findings.iter().any(|f| f.rule == "tautological-assert"),
            "assert_eq! with identical call expressions must not be flagged as tautological"
        );
    }

    #[test]
    fn flags_over_commented_block() {
        // 7 comment lines, 6 code lines → 54% comments, code ≥ the 6-line floor.
        let src = "\
// explain
// more
// even more
// and more
// and yet more
// keep going
// last one
fn a() {}
fn b() {}
fn c() {}
fn d() {}
fn e() {}
fn f() {}
";
        let r = over_comment_finding("x.rs", src, &all_lines(src));
        assert!(r.is_some(), "expected an over-comment finding");
    }

    #[test]
    fn does_not_flag_normal_comment_density() {
        // 1 comment, 6 code → well under the threshold.
        let src = "\
// one note
fn a() {}
fn b() {}
fn c() {}
fn d() {}
fn e() {}
fn f() {}
";
        assert!(over_comment_finding("x.rs", src, &all_lines(src)).is_none());
    }

    #[test]
    fn score_saturates_at_100() {
        // Many wrappers → score capped.
        let mut src = String::from("fn inner(a: u32) -> u32 { a }\n");
        for i in 0..10 {
            src.push_str(&format!("fn w{i}(a: u32) -> u32 {{ inner(a) }}\n"));
        }
        assert_eq!(scan_src(&src).score, 100);
    }

    #[test]
    fn added_lines_parses_new_side_numbers() {
        let diff = "\
diff --git a/src/x.rs b/src/x.rs
--- a/src/x.rs
+++ b/src/x.rs
@@ -1,2 +1,3 @@
 keep
+added one
+added two
@@ -10,1 +11,1 @@
-old
+changed
";
        let map = added_lines(diff);
        let lines = map.get("src/x.rs").expect("file present");
        // New-side: line 1 context, 2 and 3 added; second hunk line 11 added.
        assert!(lines.contains(&2));
        assert!(lines.contains(&3));
        assert!(lines.contains(&11));
        assert!(!lines.contains(&1));
    }

    #[test]
    fn findings_are_scoped_to_added_lines() {
        // The wrapper is on line 1, but only line 5 is "added" → not flagged.
        let src =
            "pub fn outer(a: u32) -> u32 { inner(a) }\nfn inner(a: u32) -> u32 { a }\n\n\nfn z() {}\n";
        let mut findings = Vec::new();
        let added: BTreeSet<u32> = [5].into_iter().collect();
        let ast = syn::parse_file(src).unwrap();
        SlopVisitor {
            file: "x.rs",
            added: &added,
            findings: &mut findings,
        }
        .visit_file(&ast);
        assert!(findings.is_empty());
    }

    #[test]
    fn impl_method_wrapper_is_flagged() {
        // visit_impl_item_fn must detect wrappers inside an impl block.
        let src = "struct S;\nimpl S {\n    pub fn outer(a: u32, b: u32) -> u32 { inner(a, b) }\n}\nfn inner(a: u32, b: u32) -> u32 { a + b }\n";
        let r = scan_src(src);
        assert!(
            r.findings.iter().any(|f| f.rule == "redundant-wrapper"),
            "impl method wrapper must be flagged by visit_impl_item_fn"
        );
    }

    #[test]
    fn wrapper_finding_reports_actual_line() {
        // outer is on line 3; the finding must report line 3, not line 1.
        let src = "\nfn inner(a: u32) -> u32 { a }\npub fn outer(a: u32) -> u32 { inner(a) }\n";
        let r = scan_src(src);
        let f = r
            .findings
            .iter()
            .find(|f| f.rule == "redundant-wrapper")
            .expect("should flag the wrapper on line 3");
        assert_eq!(
            f.line, 3,
            "line_of must return the wrapper's actual source line"
        );
    }

    #[test]
    fn qualified_path_arg_not_treated_as_ident() {
        // `<u32>::MAX` is an Expr::Path with qself=Some. Its single trailing
        // segment ("MAX") matches the parameter name, but the qself guard must
        // prevent it from being treated as a plain-identifier argument — the
        // function must NOT be flagged as a redundant wrapper.
        let src =
            "fn outer(MAX: u32) -> u32 { inner(<u32>::MAX) }\nfn inner(x: u32) -> u32 { x }\n";
        let r = scan_src(src);
        assert!(
            !r.findings.iter().any(|f| f.rule == "redundant-wrapper"),
            "a qualified-path arg must not be treated as a plain identifier"
        );
    }

    #[test]
    fn wrapper_finding_names_callee_in_message() {
        // path_tail must contribute the actual callee name to the message.
        let r = scan_src(
            "pub fn outer(a: u32, b: u32) -> u32 { inner(a, b) }\nfn inner(a: u32, b: u32) -> u32 { a + b }\n",
        );
        let f = r
            .findings
            .iter()
            .find(|f| f.rule == "redundant-wrapper")
            .expect("should flag the wrapper");
        assert!(
            f.message.contains("inner"),
            "finding message must name the callee (`inner`); got: {:?}",
            f.message
        );
        assert!(
            f.message.contains("outer"),
            "finding message must name the wrapper (`outer`); got: {:?}",
            f.message
        );
    }

    #[test]
    fn assert_false_not_flagged_as_tautological() {
        // assert!(false) always fails — it is not a tautology.
        let r = scan_src("#[test]\nfn t() { assert!(false); }\n");
        assert!(
            !r.findings.iter().any(|f| f.rule == "tautological-assert"),
            "assert!(false) must not be flagged as tautological"
        );
    }

    #[test]
    fn over_comment_at_exact_half_not_flagged() {
        // 6 comments + 6 code lines = exactly 50% — at the boundary, must not fire
        // (the rule fires only when ratio > 0.5, i.e. strictly more than half).
        let src = "// c1\n// c2\n// c3\n// c4\n// c5\n// c6\nfn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\nfn f() {}\n";
        assert!(
            over_comment_finding("x.rs", src, &all_lines(src)).is_none(),
            "exactly 50% comments must not trigger the over-comment rule"
        );
    }

    #[test]
    fn does_not_flag_different_variable_comparison() {
        // assert_eq!(a, b) — two DIFFERENT identifiers — is not a tautology.
        // Kills the `tokens_eq → true` mutation which would wrongly flag this.
        let r = scan_src("#[test]\nfn t() { assert_eq!(a, b); }\n");
        assert!(
            !r.findings.iter().any(|f| f.rule == "tautological-assert"),
            "assert_eq! with different variables must not be flagged as tautological"
        );
    }

    #[test]
    fn over_comment_finding_reports_first_comment_line() {
        // 6 code lines first, then 7 comment lines; first comment is on line 7.
        // Kills the `== → !=` mutation in the `if first_comment_line == 0` guard.
        let src = "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\nfn f() {}\n// c1\n// c2\n// c3\n// c4\n// c5\n// c6\n// c7\n";
        let r = over_comment_finding("x.rs", src, &all_lines(src))
            .expect("should flag over-commented block");
        assert_eq!(
            r.line, 7,
            "finding must report the actual first comment line"
        );
    }

    #[test]
    fn scan_files_returns_findings_for_real_file() {
        use tempfile::tempdir;
        // scan_files reads actual files from disk — exercise it directly to
        // ensure it returns findings rather than an empty default.
        let dir = tempdir().unwrap();
        let src = "pub fn outer(a: u32) -> u32 { inner(a) }\nfn inner(a: u32) -> u32 { a }\n";
        std::fs::write(dir.path().join("x.rs"), src).unwrap();

        let files = vec!["x.rs".to_string()];
        let diff = "--- /dev/null\n+++ b/x.rs\n@@ -0,0 +1,2 @@\n+pub fn outer(a: u32) -> u32 { inner(a) }\n+fn inner(a: u32) -> u32 { a }\n";
        let added = added_lines(diff);
        let report = scan_files(dir.path(), &files, &added);
        assert!(
            !report.findings.is_empty(),
            "scan_files must return findings for a file with slop patterns"
        );
    }
}
