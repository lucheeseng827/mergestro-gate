//! The README quickstarts, verbatim: docs that no longer compile are worse than no docs.
//! Lives in the CLI crate because it is the one that depends on every other.
use turnover_core::line::heuristic_mask;
use turnover_core::{classify, CommitInput, FileChange, Language, Origin, SignalConfig};
use turnover_lang::{mask_for_path, MaskSource};

#[test]
fn core_readme_quickstart() {
    let body = "fn total(items: &[u64]) -> u64 {\n    items.iter().sum()\n}\n";
    let commit = CommitInput {
        sha: "abc123".into(),
        parent: None,
        timestamp_unix: 0,
        author: "dev@example.com".into(),
        is_merge: false,
        origin: Origin::Human,
        files: vec![FileChange {
            path: "src/lib.rs".into(),
            language: Language::Rust,
            old: None,
            new: Some(body.to_string()),
            old_mask: None,
            new_mask: Some(heuristic_mask(Language::Rust, body)),
        }],
    };

    let signals = classify(&commit, &SignalConfig::default());
    assert_eq!(signals.counts.added, 2); // the bare `}` is punctuation, never significant
}

#[test]
fn lang_readme_quickstart() {
    let src = "// a note\nlet x = 1;\n";
    let (mask, source) = mask_for_path(Language::Rust, "src/lib.rs", src);

    assert_eq!(source, MaskSource::Parsed);
    assert_eq!(mask, vec![false, true]); // the comment does not count, the binding does
}

// Compile-only: these two need a real repository to run, but their API surface is exactly
// what the history and gate READMEs print, so a signature change breaks the build here.
#[allow(dead_code)]
mod compiles {
    use turnover_core::SignalConfig;
    use turnover_history::{classify_all, list_commits, open, Error, WalkOptions};

    fn walk_head() -> Result<(), Error> {
        let repo = open(".")?;
        let opts = WalkOptions {
            tips: vec!["HEAD".to_string()],
            ..Default::default()
        };
        let metas = list_commits(&repo, &opts)?;
        let (rows, stats) =
            classify_all(&repo, &metas, &opts, &SignalConfig::default(), &|_done| {})?;

        println!(
            "{} commits, {} rows, {} lines parsed",
            metas.len(),
            rows.len(),
            stats.lines_parsed
        );
        Ok(())
    }

    use turnover_gate::{render, run_gate, Config, GateError, GateRequest, Scope};

    fn gate_this_pr() -> Result<(), GateError> {
        let mut req = GateRequest::new(".", Config::default());
        req.scope = Scope::BaseRef("origin/main".to_string());
        req.update_baseline = false;

        let outcome = run_gate(&req)?;
        println!("{}", render::text(&outcome));
        Ok(())
    }
}
