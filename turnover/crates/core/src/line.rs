//! What counts as a line. Every ratio in the model is "X significant added lines over all
//! significant added lines", so this file decides the denominator — and the denominator is
//! where cross-language comparability is won or lost. A Go file's `}` lines and a Python
//! file's docstrings must not count, or Go repos look busier and Python repos look duplicated.
//!
//! Two inputs feed a per-line [`Mask`]: a *comment* mask (which byte ranges are comments,
//! from a real parser when `turnover-lang` supplies one) and a text test that rejects blank
//! and punctuation-only lines. [`heuristic_mask`] is the parser-free fallback and the thing
//! `Other` languages get.

use crate::language::Language;

/// Per-line significance: `mask[i]` is `true` iff line `i` of the text counts.
pub type Mask = Vec<bool>;

/// Normalise a line for identity comparison: trim, collapse internal whitespace to one
/// space. Case and punctuation are kept — `x = 1` and `x=1` are different lines to a
/// reader, and the model would rather under-count duplication than invent it.
pub fn normalize(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut pending_space = false;
    for ch in line.trim().chars() {
        if ch.is_whitespace() {
            pending_space = true;
        } else {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(ch);
        }
    }
    out
}

/// `true` if the normalised text carries enough identity to be worth matching. The bar is
/// low on purpose: the mask already dropped comments and blanks; this only rejects the
/// structural residue (`}`, `);`, `end`, `else:`) and lone keywords that every file repeats.
pub fn is_significant_text(normalized: &str) -> bool {
    let alnum = normalized.chars().filter(|c| c.is_alphanumeric()).count();
    if alnum < 2 {
        return false;
    }
    // At least one identifier-like token of 3+ chars that is not a bare structural keyword.
    normalized
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|tok| tok.len() >= 3)
        .any(|tok| !STRUCTURAL.contains(&tok))
}

/// Tokens that make a line structural rather than meaningful when they are all it has.
const STRUCTURAL: &[&str] = &[
    "else", "return", "break", "continue", "pass", "end", "try", "default", "None", "null", "nil",
    "true", "false", "True", "False", "self", "this", "elif", "except", "finally", "catch", "then",
    "done", "esac", "endif", "begin", "case", "switch", "loop", "unsafe", "async", "await",
    "yield", "match", "for", "while",
];

/// `true` if the normalised line is an import-style statement for `lang`.
pub fn is_import(lang: Language, normalized: &str) -> bool {
    lang.import_prefixes()
        .iter()
        .any(|p| normalized.starts_with(p))
}

/// Build a mask from a comment-range oracle. `comment_ranges` are byte ranges (in `text`)
/// that a parser identified as comments; they may nest or overlap and need not be sorted.
/// A line is significant iff, with its comment bytes blanked, it still passes
/// [`is_significant_text`].
pub fn mask_with_comments(text: &str, comment_ranges: &[(usize, usize)]) -> Mask {
    let mut ranges: Vec<(usize, usize)> = comment_ranges.to_vec();
    ranges.sort_unstable();
    let mut mask = Vec::new();
    let mut offset = 0usize;
    let mut ri = 0usize;
    for line in text.split_inclusive('\n') {
        let start = offset;
        let end = offset + line.len();
        offset = end;
        // Advance past ranges that ended before this line.
        while ri < ranges.len() && ranges[ri].1 <= start {
            ri += 1;
        }
        let mut visible = String::with_capacity(line.len());
        let mut pos = start;
        let mut k = ri;
        while k < ranges.len() && ranges[k].0 < end {
            let (rs, re) = ranges[k];
            let rs = rs.max(start);
            let re = re.min(end);
            if rs > pos {
                visible.push_str(&text[pos..rs]);
            }
            pos = pos.max(re);
            k += 1;
        }
        if pos < end {
            visible.push_str(&text[pos..end]);
        }
        mask.push(is_significant_text(&normalize(&visible)));
    }
    mask
}

/// Parser-free significance. Drops blank lines, lines that *start* with one of the language's
/// line-comment prefixes, and lines with no identity. It cannot see block comments that start
/// mid-line or docstrings, which is exactly the gap the tree-sitter mask closes.
pub fn heuristic_mask(lang: Language, text: &str) -> Mask {
    text.split_inclusive('\n')
        .map(|raw| {
            let n = normalize(raw);
            if n.is_empty() {
                return false;
            }
            if lang
                .line_comment_prefixes()
                .iter()
                .any(|p| n.starts_with(p))
            {
                return false;
            }
            is_significant_text(&n)
        })
        .collect()
}

/// Number of lines `split_inclusive('\n')` yields for `text` — the length every mask must have.
pub fn line_count(text: &str) -> usize {
    text.split_inclusive('\n').count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_whitespace_only() {
        assert_eq!(normalize("  let   x =\t1;  "), "let x = 1;");
        assert_eq!(normalize("\n"), "");
    }

    #[test]
    fn structural_lines_are_not_significant() {
        for s in [
            "}", ");", "end", "else:", "} else {", "return", "break;", "{", "]",
        ] {
            assert!(
                !is_significant_text(&normalize(s)),
                "{s:?} should be insignificant"
            );
        }
        for s in [
            "let total = price * qty;",
            "return compute(x)",
            "if err != nil {",
        ] {
            assert!(
                is_significant_text(&normalize(s)),
                "{s:?} should be significant"
            );
        }
    }

    #[test]
    fn heuristic_mask_drops_comments_and_blanks() {
        let text = "// header\nfn main() {\n\n    let x = compute();\n}\n";
        assert_eq!(
            heuristic_mask(Language::Rust, text),
            vec![false, true, false, true, false]
        );
    }

    #[test]
    fn comment_ranges_blank_out_inline_and_block_comments() {
        let text = "let a = 1; /* start\n still comment\n end */ let b = 2;\n";
        let start = text.find("/*").unwrap();
        let end = text.find("*/").unwrap() + 2;
        let mask = mask_with_comments(text, &[(start, end)]);
        assert_eq!(mask, vec![true, false, true]);
    }

    #[test]
    fn imports_are_detected_per_language() {
        assert!(is_import(Language::Rust, "use std::fmt;"));
        assert!(is_import(Language::Python, "from x import y"));
        assert!(!is_import(Language::Python, "fromage = 1"));
        assert!(is_import(Language::Go, "package main"));
    }
}
