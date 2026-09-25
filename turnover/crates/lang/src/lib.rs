//! turnover-lang — the tree-sitter significance oracle. Parses a file once and reports which
//! byte ranges are comments (and, for Python, docstrings), so `turnover-core` can build a
//! per-line mask that does not depend on how a language spells its comments.
//!
//! Only the mask crosses the boundary. The AST never reaches the classifier, on purpose: the
//! three v0 signals are line-identity signals, and pinning them to syntax would make them
//! incomparable across languages the moment one grammar is missing. The grammars compiled
//! in here are the ones the walker will see most in polyglot repositories; every other
//! [`Language`] falls back to [`turnover_core::line::heuristic_mask`], and the report says
//! how many lines came from which path so the fallback's blind spots are visible.
//!
//! Error-masking detection (bare `except`, swallowed `catch`, `unwrap()` on fresh error paths)
//! is the next consumer of the tree and is where this crate grows.

use std::cell::RefCell;

use tree_sitter::{Language as TsLanguage, Node, Parser};
use turnover_core::line::{heuristic_mask, line_count, mask_with_comments, Mask};
use turnover_core::Language;

/// How a mask was produced — reported per file so a reader knows what the number rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskSource {
    /// A grammar parsed the file; comment and docstring ranges are exact.
    Parsed,
    /// The grammar is not compiled in, or parsing failed: line-prefix heuristic.
    Heuristic,
}

/// The grammar for a language. TypeScript has two: `.tsx` files carry JSX and need the TSX
/// grammar, which is why the path travels with the language.
fn grammar(lang: Language, path: &str) -> Option<TsLanguage> {
    Some(match lang {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::TypeScript => {
            if path.to_ascii_lowercase().ends_with(".tsx") {
                tree_sitter_typescript::LANGUAGE_TSX.into()
            } else {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            }
        }
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        _ => return None,
    })
}

/// `true` if a grammar is compiled in for `lang`.
pub fn has_grammar(lang: Language) -> bool {
    grammar(lang, "").is_some()
}

/// The languages with a compiled-in grammar, for the report.
pub const GRAMMARS: &[Language] = &[
    Language::Rust,
    Language::Python,
    Language::JavaScript,
    Language::TypeScript,
    Language::Go,
    Language::Java,
];

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(Parser::new());
}

/// Byte ranges of comments (and docstrings) in `text`, or `None` if no grammar applies.
pub fn comment_ranges(lang: Language, text: &str) -> Option<Vec<(usize, usize)>> {
    comment_ranges_for_path(lang, "", text)
}

/// [`comment_ranges`] with the file path, which picks the TSX grammar for `.tsx`.
pub fn comment_ranges_for_path(
    lang: Language,
    path: &str,
    text: &str,
) -> Option<Vec<(usize, usize)>> {
    let grammar = grammar(lang, path)?;
    PARSER.with(|p| {
        let mut parser = p.borrow_mut();
        parser.set_language(&grammar).ok()?;
        let tree = parser.parse(text, None)?;
        let mut out = Vec::new();
        collect(tree.root_node(), lang, text, &mut out);
        Some(out)
    })
}

fn collect(node: Node<'_>, lang: Language, text: &str, out: &mut Vec<(usize, usize)>) {
    let kind = node.kind();
    let is_comment = matches!(
        kind,
        "comment" | "line_comment" | "block_comment" | "doc_comment"
    );
    if is_comment {
        out.push((node.start_byte(), node.end_byte()));
        return;
    }
    // Python docstrings: an expression_statement whose only child is a string literal, as the
    // first statement of a module/class/function body. Treating every bare string statement
    // as documentation is the simpler, stricter rule and matches how they are read.
    if lang == Language::Python && kind == "expression_statement" && node.named_child_count() == 1 {
        if let Some(child) = node.named_child(0) {
            if child.kind() == "string" {
                out.push((node.start_byte(), node.end_byte()));
                return;
            }
        }
    }
    let _ = text;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, lang, text, out);
    }
}

/// The significance mask for `text`, and how it was produced.
pub fn mask(lang: Language, text: &str) -> (Mask, MaskSource) {
    mask_for_path(lang, "", text)
}

/// [`mask`] with the file path, which picks the TSX grammar for `.tsx`.
pub fn mask_for_path(lang: Language, path: &str, text: &str) -> (Mask, MaskSource) {
    match comment_ranges_for_path(lang, path, text) {
        Some(ranges) => {
            let m = mask_with_comments(text, &ranges);
            debug_assert_eq!(m.len(), line_count(text));
            (m, MaskSource::Parsed)
        }
        None => (heuristic_mask(lang, text), MaskSource::Heuristic),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_block_and_doc_comments_are_not_significant() {
        let text = "/// docs\nfn main() {\n    /* multi\n       line */ let x = compute();\n    let y = 2; // trailing\n}\n";
        let (m, src) = mask(Language::Rust, text);
        assert_eq!(src, MaskSource::Parsed);
        assert_eq!(m, vec![false, true, false, true, true, false]);
    }

    #[test]
    fn python_docstrings_and_hash_comments_are_not_significant() {
        let text = "def f(x):\n    \"\"\"Docstring\n    over lines\n    \"\"\"\n    # comment\n    return compute(x)\n";
        let (m, src) = mask(Language::Python, text);
        assert_eq!(src, MaskSource::Parsed);
        assert_eq!(m, vec![true, false, false, false, false, true]);
    }

    #[test]
    fn javascript_and_go_parse() {
        let (m, src) = mask(
            Language::JavaScript,
            "// c\nconst total = items.reduce(sum, 0);\n",
        );
        assert_eq!((m, src), (vec![false, true], MaskSource::Parsed));
        let (m, src) = mask(
            Language::Go,
            "package main\n/* c */\nfunc main() { compute() }\n",
        );
        assert_eq!((m, src), (vec![true, false, true], MaskSource::Parsed));
    }

    #[test]
    fn typescript_tsx_and_java_parse() {
        let ts = "// c\ninterface Item { price: number }\n/** doc */\nconst total = (xs: Item[]) => xs.reduce((a, b) => a + b.price, 0);\n";
        let (m, src) = mask_for_path(Language::TypeScript, "a.ts", ts);
        assert_eq!(src, MaskSource::Parsed);
        assert_eq!(m, vec![false, true, false, true]);
        let tsx = "export const View = () => (\n  <div>{/* jsx comment */}</div>\n);\n";
        let (m, src) = mask_for_path(Language::TypeScript, "View.tsx", tsx);
        assert_eq!(src, MaskSource::Parsed);
        assert_eq!(
            m,
            vec![true, true, false],
            "the JSX line keeps its tags; the comment inside is masked but `div` remains"
        );
        let java = "package a;\n/** javadoc */\npublic class A {\n    // c\n    int total(int[] xs) { return xs.length; }\n}\n";
        let (m, src) = mask_for_path(Language::Java, "A.java", java);
        assert_eq!(src, MaskSource::Parsed);
        assert_eq!(m, vec![true, false, true, false, true, false]);
        assert!(GRAMMARS.contains(&Language::Java));
    }

    #[test]
    fn unknown_languages_fall_back_to_the_heuristic() {
        let (m, src) = mask(Language::Ruby, "# c\nputs compute(x)\n");
        assert_eq!(src, MaskSource::Heuristic);
        assert_eq!(m, vec![false, true]);
    }
}
