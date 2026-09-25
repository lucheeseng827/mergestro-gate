//! Language identity. Lives in core (not the tree-sitter crate) because the classifier and
//! the report both key on it, and neither should need a C build to name a language.

use serde::{Deserialize, Serialize};

/// The languages the signal model knows how to count comparably. `Other` is any text file the
/// walker chose to include anyway; it is counted with the heuristic mask and reported under its
/// own key so a polyglot repo can see how much of its number rests on unparsed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
    Java,
    Kotlin,
    C,
    Cpp,
    CSharp,
    Ruby,
    Php,
    Swift,
    Scala,
    Shell,
    Other,
}

impl Language {
    /// Classify by file extension. Returns `None` for paths the gate should never count:
    /// data, markup, lockfiles, minified bundles and generated artefacts. Those are not "code
    /// getting worse"; a 4,000-line lockfile bump would drown every ratio in the window.
    pub fn from_path(path: &str) -> Option<Language> {
        let lower = path.to_ascii_lowercase();
        let file = lower.rsplit('/').next().unwrap_or(&lower);
        if is_generated_name(file) {
            return None;
        }
        let ext = file.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        let lang = match ext {
            "rs" => Language::Rust,
            "py" | "pyi" => Language::Python,
            "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
            "ts" | "tsx" | "mts" | "cts" => Language::TypeScript,
            "go" => Language::Go,
            "java" => Language::Java,
            "kt" | "kts" => Language::Kotlin,
            "c" | "h" => Language::C,
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Language::Cpp,
            "cs" => Language::CSharp,
            "rb" => Language::Ruby,
            "php" => Language::Php,
            "swift" => Language::Swift,
            "scala" | "sc" => Language::Scala,
            "sh" | "bash" | "zsh" => Language::Shell,
            _ => return None,
        };
        Some(lang)
    }

    /// The stable report key.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Go => "go",
            Language::Java => "java",
            Language::Kotlin => "kotlin",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::CSharp => "csharp",
            Language::Ruby => "ruby",
            Language::Php => "php",
            Language::Swift => "swift",
            Language::Scala => "scala",
            Language::Shell => "shell",
            Language::Other => "other",
        }
    }

    /// Line prefixes that introduce a comment for this language, for the heuristic mask.
    pub fn line_comment_prefixes(self) -> &'static [&'static str] {
        match self {
            Language::Python | Language::Ruby | Language::Shell => &["#"],
            Language::Php => &["//", "#", "*", "/*"],
            Language::Other => &["//", "#", "*", "/*", "--", ";"],
            _ => &["//", "*", "/*"],
        }
    }

    /// Import-style statements. Excluded from copy/paste and duplicate-block counting: two
    /// files sharing `use std::collections::HashMap;` is not duplication anyone would fix.
    pub fn import_prefixes(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &[
                "use ",
                "pub use ",
                "pub(crate) use ",
                "extern crate ",
                "mod ",
                "pub mod ",
            ],
            Language::Python => &["import ", "from "],
            Language::JavaScript | Language::TypeScript => &[
                "import ",
                "export * ",
                "export {",
                "export type ",
                "require(",
            ],
            Language::Go => &["import ", "package "],
            Language::Java | Language::Kotlin | Language::Scala => &["import ", "package "],
            Language::C | Language::Cpp => &["#include", "#pragma", "#define"],
            Language::CSharp => &["using ", "namespace "],
            Language::Ruby => &["require ", "require_relative "],
            Language::Php => &[
                "use ",
                "require ",
                "require_once ",
                "include ",
                "namespace ",
            ],
            Language::Swift => &["import "],
            Language::Shell => &["source ", ". "],
            Language::Other => &["import ", "use ", "#include", "require"],
        }
    }
}

/// Names the walker should never count, whatever their extension says.
fn is_generated_name(file: &str) -> bool {
    const EXACT: &[&str] = &[
        "cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "go.sum",
        "poetry.lock",
        "gemfile.lock",
        "composer.lock",
    ];
    if EXACT.contains(&file) {
        return true;
    }
    file.ends_with(".min.js")
        || file.ends_with(".min.css")
        || file.ends_with(".pb.go")
        || file.ends_with(".pb.rs")
        || file.ends_with("_pb2.py")
        || file.ends_with(".generated.ts")
        || file.ends_with(".g.cs")
        || file.ends_with(".d.ts")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_map_and_generated_files_are_excluded() {
        assert_eq!(Language::from_path("src/main.rs"), Some(Language::Rust));
        assert_eq!(Language::from_path("a/b/c.py"), Some(Language::Python));
        assert_eq!(
            Language::from_path("web/app.TSX"),
            Some(Language::TypeScript)
        );
        assert_eq!(Language::from_path("Cargo.lock"), None);
        assert_eq!(Language::from_path("dist/bundle.min.js"), None);
        assert_eq!(Language::from_path("api/v1/thing.pb.go"), None);
        assert_eq!(Language::from_path("README.md"), None);
        assert_eq!(Language::from_path("Makefile"), None);
    }
}
