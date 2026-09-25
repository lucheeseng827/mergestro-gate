//! Token abstraction for rename-insensitive (Type-2) clone detection.
//!
//! Exact line matching catches code pasted verbatim. The dominant AI pattern is paste-then-
//! rename: the same function with `total` renamed to `sum` and `7` changed to `8`, which
//! exact matching never sees. Abstracting a line to its shape — keywords and punctuation
//! kept, identifiers replaced by `$`, literals by `#` — makes those lines equal again.
//!
//! The price is false structure: `x = 1;` and `y = 2;` abstract to the same thing, so an
//! abstracted window is only a clone when it carries enough tokens to have identity
//! ([`crate::signals::SignalConfig::block_min_tokens`]). Keywords stay concrete because they
//! are the shape: a `for` is not a `while` after renaming.

use crate::language::Language;

/// Abstract a normalised line: keywords and punctuation verbatim, identifiers `$`, numbers
/// and string/char literals `#`. Returns the abstracted text and the token count (comments
/// trailing the code are dropped first for languages with line comments).
pub fn abstract_line(lang: Language, line: &str) -> (String, usize) {
    let code = strip_trailing_comment(lang, line);
    let keywords = keywords(lang);
    let mut out = String::with_capacity(code.len());
    let mut tokens = 0usize;
    let chars: Vec<char> = code.chars().collect();
    let mut i = 0;
    let push = |out: &mut String, s: &str| {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(s);
    };
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if keywords.contains(&word.as_str()) {
                push(&mut out, &word);
            } else {
                push(&mut out, "$");
            }
            tokens += 1;
            continue;
        }
        if c.is_ascii_digit() {
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                i += 1;
            }
            push(&mut out, "#");
            tokens += 1;
            continue;
        }
        if c == '"' || c == '`' || (c == '\'' && opens_char_or_string(lang, &chars[i + 1..])) {
            let quote = c;
            i += 1;
            while i < chars.len() && chars[i] != quote {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1; // closing quote (or end)
            push(&mut out, "#");
            tokens += 1;
            continue;
        }
        // Punctuation: one token per char, kept verbatim.
        let mut s = String::new();
        s.push(c);
        push(&mut out, &s);
        tokens += 1;
        i += 1;
    }
    (out, tokens)
}

/// Drop a trailing line comment (outside string literals). Walks char boundaries, never
/// byte offsets: a comment after an em-dash must not be a panic.
/// Does an apostrophe at this position open a literal, or is it a lifetime / an English
/// apostrophe? Only a closing quote makes it a literal — and in the languages where `'`
/// delimits a *character* rather than a string, only one close enough to be one character
/// (`'\u{10FFFF}'` is the longest body those languages admit). Without this, `&'a str` and
/// `it doesn't` swallowed the rest of the line, collapsing distinct lines to one shape.
fn opens_char_or_string(lang: Language, rest: &[char]) -> bool {
    match lang {
        // `'` delimits a *character* here, so a literal is `'x'` or an escape `'\n'`,
        // `'\u{1F600}'` — anything else is a lifetime (`&'a str`) and stays punctuation.
        Language::Rust
        | Language::C
        | Language::Cpp
        | Language::Java
        | Language::Kotlin
        | Language::Scala
        | Language::Swift
        | Language::Go => match rest.first() {
            Some('\\') => rest.iter().take(12).skip(1).any(|&c| c == '\''),
            Some(_) => rest.get(1) == Some(&'\''),
            None => false,
        },
        // Everywhere else `'` opens a string: it is a literal as soon as the line closes it.
        _ => rest.contains(&'\''),
    }
}

fn strip_trailing_comment(lang: Language, line: &str) -> &str {
    let markers: &[&str] = match lang {
        Language::Python | Language::Ruby | Language::Shell => &["#"],
        Language::Php => &["//", "#"],
        Language::Other => &["//", "#"],
        _ => &["//"],
    };
    let mut in_str: Option<char> = None;
    let mut skip_next = false;
    for (i, c) in line.char_indices() {
        if skip_next {
            skip_next = false;
            continue;
        }
        match in_str {
            Some(q) => {
                if c == '\\' {
                    skip_next = true;
                } else if c == q {
                    in_str = None;
                }
            }
            None => {
                if c == '"'
                    || c == '`'
                    || (c == '\''
                        && opens_char_or_string(lang, &line[i + 1..].chars().collect::<Vec<_>>()))
                {
                    in_str = Some(c);
                } else if markers.iter().any(|m| line[i..].starts_with(m)) {
                    return line[..i].trim_end();
                }
            }
        }
    }
    line
}

/// The reserved words that stay concrete under abstraction.
pub fn keywords(lang: Language) -> &'static [&'static str] {
    match lang {
        Language::Rust => &[
            "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
            "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
            "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
            "trait", "true", "type", "unsafe", "use", "where", "while", "Some", "None", "Ok",
            "Err",
        ],
        Language::Python => &[
            "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class",
            "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global",
            "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise",
            "return", "try", "while", "with", "yield", "self", "cls",
        ],
        Language::JavaScript | Language::TypeScript => &[
            "async",
            "await",
            "break",
            "case",
            "catch",
            "class",
            "const",
            "continue",
            "debugger",
            "default",
            "delete",
            "do",
            "else",
            "enum",
            "export",
            "extends",
            "false",
            "finally",
            "for",
            "function",
            "if",
            "implements",
            "import",
            "in",
            "instanceof",
            "interface",
            "let",
            "new",
            "null",
            "of",
            "package",
            "private",
            "protected",
            "public",
            "return",
            "static",
            "super",
            "switch",
            "this",
            "throw",
            "true",
            "try",
            "type",
            "typeof",
            "undefined",
            "var",
            "void",
            "while",
            "with",
            "yield",
            "readonly",
            "as",
            "declare",
            "namespace",
            "abstract",
        ],
        Language::Go => &[
            "break",
            "case",
            "chan",
            "const",
            "continue",
            "default",
            "defer",
            "else",
            "fallthrough",
            "for",
            "func",
            "go",
            "goto",
            "if",
            "import",
            "interface",
            "map",
            "package",
            "range",
            "return",
            "select",
            "struct",
            "switch",
            "type",
            "var",
            "nil",
            "true",
            "false",
            "err",
            "make",
            "new",
            "len",
            "append",
            "error",
        ],
        Language::Java | Language::Kotlin | Language::Scala => &[
            "abstract",
            "assert",
            "boolean",
            "break",
            "byte",
            "case",
            "catch",
            "char",
            "class",
            "const",
            "continue",
            "default",
            "do",
            "double",
            "else",
            "enum",
            "extends",
            "final",
            "finally",
            "float",
            "for",
            "if",
            "implements",
            "import",
            "instanceof",
            "int",
            "interface",
            "long",
            "native",
            "new",
            "null",
            "package",
            "private",
            "protected",
            "public",
            "return",
            "short",
            "static",
            "strictfp",
            "super",
            "switch",
            "synchronized",
            "this",
            "throw",
            "throws",
            "transient",
            "try",
            "void",
            "volatile",
            "while",
            "true",
            "false",
            "var",
            "val",
            "fun",
            "when",
            "object",
            "override",
            "data",
            "sealed",
            "is",
            "in",
            "def",
            "match",
            "case",
            "yield",
            "trait",
        ],
        Language::C | Language::Cpp | Language::CSharp => &[
            "auto",
            "break",
            "case",
            "char",
            "const",
            "continue",
            "default",
            "do",
            "double",
            "else",
            "enum",
            "extern",
            "float",
            "for",
            "goto",
            "if",
            "inline",
            "int",
            "long",
            "register",
            "return",
            "short",
            "signed",
            "sizeof",
            "static",
            "struct",
            "switch",
            "typedef",
            "union",
            "unsigned",
            "void",
            "volatile",
            "while",
            "class",
            "namespace",
            "new",
            "delete",
            "this",
            "template",
            "typename",
            "public",
            "private",
            "protected",
            "virtual",
            "override",
            "using",
            "try",
            "catch",
            "throw",
            "true",
            "false",
            "nullptr",
            "null",
            "bool",
            "string",
            "var",
            "async",
            "await",
            "foreach",
            "in",
            "is",
            "as",
            "readonly",
            "internal",
            "sealed",
            "abstract",
            "interface",
            "base",
            "get",
            "set",
        ],
        Language::Ruby => &[
            "alias",
            "and",
            "begin",
            "break",
            "case",
            "class",
            "def",
            "defined?",
            "do",
            "else",
            "elsif",
            "end",
            "ensure",
            "false",
            "for",
            "if",
            "in",
            "module",
            "next",
            "nil",
            "not",
            "or",
            "redo",
            "rescue",
            "retry",
            "return",
            "self",
            "super",
            "then",
            "true",
            "undef",
            "unless",
            "until",
            "when",
            "while",
            "yield",
            "require",
            "attr_reader",
            "attr_accessor",
            "puts",
        ],
        Language::Php => &[
            "abstract",
            "and",
            "array",
            "as",
            "break",
            "callable",
            "case",
            "catch",
            "class",
            "clone",
            "const",
            "continue",
            "declare",
            "default",
            "do",
            "echo",
            "else",
            "elseif",
            "empty",
            "extends",
            "final",
            "finally",
            "fn",
            "for",
            "foreach",
            "function",
            "global",
            "if",
            "implements",
            "include",
            "instanceof",
            "interface",
            "isset",
            "list",
            "match",
            "namespace",
            "new",
            "null",
            "or",
            "private",
            "protected",
            "public",
            "readonly",
            "require",
            "return",
            "static",
            "switch",
            "throw",
            "trait",
            "try",
            "unset",
            "use",
            "var",
            "while",
            "yield",
            "true",
            "false",
            "this",
            "self",
        ],
        Language::Swift => &[
            "associatedtype",
            "class",
            "deinit",
            "enum",
            "extension",
            "fileprivate",
            "func",
            "import",
            "init",
            "inout",
            "internal",
            "let",
            "open",
            "operator",
            "private",
            "protocol",
            "public",
            "static",
            "struct",
            "subscript",
            "typealias",
            "var",
            "break",
            "case",
            "continue",
            "default",
            "defer",
            "do",
            "else",
            "fallthrough",
            "for",
            "guard",
            "if",
            "in",
            "repeat",
            "return",
            "switch",
            "where",
            "while",
            "as",
            "catch",
            "false",
            "is",
            "nil",
            "rethrows",
            "self",
            "Self",
            "super",
            "throw",
            "throws",
            "true",
            "try",
            "async",
            "await",
            "some",
            "any",
        ],
        Language::Shell => &[
            "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "until",
            "case", "esac", "function", "return", "local", "export", "echo", "exit", "set", "true",
            "false",
        ],
        Language::Other => &[
            "if", "else", "for", "while", "return", "function", "def", "fn", "class", "true",
            "false", "null", "nil", "none", "import", "let", "const", "var",
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_and_literals_abstract_but_keywords_and_shape_stay() {
        let (a, n) = abstract_line(Language::Rust, "let mut total = items.len() * 7; // count");
        assert_eq!(a, "let mut $ = $ . $ ( ) * # ;");
        assert_eq!(n, 12);
        let (b, _) = abstract_line(Language::Rust, "let mut sum = rows.len() * 12;");
        assert_eq!(a, b, "a rename and a literal change are the same shape");
        let (c, _) = abstract_line(Language::Rust, "while total > 0 {");
        assert_eq!(c, "while $ > # {");
    }

    #[test]
    fn strings_are_one_literal_and_comment_markers_inside_them_are_kept() {
        let (a, n) = abstract_line(Language::Python, "url = \"http://x/#frag\"  # trailing");
        assert_eq!(a, "$ = #");
        assert_eq!(n, 3);
        let (b, _) = abstract_line(Language::JavaScript, "const s = `a ${b} c`; // c");
        assert_eq!(b, "const $ = # ;");
    }

    #[test]
    fn multibyte_text_never_panics_the_stripper() {
        let (a, n) = abstract_line(
            Language::Rust,
            "with dagron Enterprise — https://x/y#z. // note — here",
        );
        assert_eq!(
            a, "$ $ $ — $ :",
            "the `//` of the bare URL reads as a comment marker, as it would to the language"
        );
        assert!(n > 0);
        let (b, _) = abstract_line(Language::Python, "s = \"héllo — wörld\"  # ünïcode");
        assert_eq!(b, "$ = #");
    }

    #[test]
    fn a_lone_apostrophe_is_not_a_string_open() {
        // A Rust lifetime and an English apostrophe both used to swallow the rest of the line
        // as a string literal, collapsing every distinct line after them to the same shape.
        let (a, _) = abstract_line(
            Language::Rust,
            "fn parse<'a>(src: &'a str) -> Cow<'a, str> {",
        );
        assert_eq!(
            a, "fn $ < ' $ > ( $ : & ' $ $ ) - > $ < ' $ , $ > {",
            "a lifetime is punctuation plus an identifier, never a literal"
        );
        let (b, _) = abstract_line(Language::Rust, "// it doesn't matter");
        assert_eq!(b, "", "a comment is still stripped, apostrophe or not");
        let (c, _) = abstract_line(Language::Rust, "let ch = 'x';");
        assert_eq!(
            c, "let $ = # ;",
            "a closed char literal is still one literal"
        );
    }

    #[test]
    fn different_control_flow_is_a_different_shape() {
        let (a, _) = abstract_line(Language::Go, "for i := range items {");
        let (b, _) = abstract_line(Language::Go, "if i := range items {");
        assert_ne!(a, b);
    }
}
