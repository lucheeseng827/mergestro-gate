//! End-to-end walk over a synthetic repository built with the `git` CLI: the classifier's
//! buckets must come out of real commits the way the unit tests say they come out of strings.

use std::path::Path;
use std::process::Command;

use turnover_core::signals::SignalConfig;
use turnover_history::{classify_all, list_commits, open, WalkOptions};

/// Keep the developer's own git config out of the fixture: no signing prompt, no hooks,
/// and (via the env below) no global or system config at all.
const ISOLATE: [&str; 4] = [
    "-c",
    "commit.gpgsign=false",
    "-c",
    "core.hooksPath=/dev/null",
];

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(ISOLATE)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn commit(dir: &Path, msg: &str, date: &str) {
    git(dir, &["add", "-A"]);
    let out = Command::new("git")
        .args(ISOLATE)
        .args(["commit", "-q", "-m", msg])
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

const BODY: &str = "fn compute_total(items: &[Item]) -> u64 {\n    let mut total = 0;\n    for item in items {\n        total += item.price * item.qty;\n    }\n    total\n}\n";

#[test]
fn walk_classifies_real_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);

    // c1: plain addition (+ a lockfile that must be ignored, + a vendored file).
    std::fs::write(dir.join("a.rs"), BODY).unwrap();
    std::fs::write(dir.join("Cargo.lock"), "[[package]]\nname = \"x\"\n").unwrap();
    std::fs::create_dir_all(dir.join("vendor")).unwrap();
    std::fs::write(dir.join("vendor/v.rs"), BODY).unwrap();
    commit(dir, "c1", "2026-01-01T00:00:00Z");

    // c2: move the function to another file (refactor).
    std::fs::write(dir.join("a.rs"), "").unwrap();
    std::fs::write(dir.join("b.rs"), BODY).unwrap();
    commit(dir, "c2", "2026-01-02T00:00:00Z");

    // c3: paste a second copy into b.rs (copy/paste + block dup with block_min_lines=3),
    //     and add a python file with a docstring.
    let twice = format!("{BODY}\n{}", BODY.replace("compute_total", "compute_again"));
    std::fs::write(dir.join("b.rs"), &twice).unwrap();
    std::fs::write(
        dir.join("m.py"),
        "def f(x):\n    \"\"\"doc\"\"\"\n    return compute_value(x)\n",
    )
    .unwrap();
    commit(dir, "c3", "2026-01-03T00:00:00Z");

    // c4: delete the pasted copy again five days later -> churn charged to c3.
    std::fs::write(dir.join("b.rs"), BODY).unwrap();
    commit(dir, "c4", "2026-01-08T00:00:00Z");

    let repo = open(dir.to_str().unwrap()).unwrap();
    let opts = WalkOptions::default();
    let metas = list_commits(&repo, &opts).unwrap();
    assert_eq!(metas.len(), 4);
    assert!(
        metas[0].timestamp_unix > metas[3].timestamp_unix,
        "newest first"
    );

    let cfg = SignalConfig {
        block_min_lines: 3,
        ..Default::default()
    };
    let (rows, stats) = classify_all(&repo, &metas, &opts, &cfg, &|_| {}).unwrap();
    assert_eq!(stats.commits, 4);
    assert_eq!(stats.files_ignored_path, 1, "vendor/ is ignored");
    assert_eq!(stats.files_unsupported, 1, "Cargo.lock is never counted");
    assert!(stats.lines_parsed > 0 && stats.lines_heuristic == 0);
    assert_eq!(rows.len(), 4, "one classified row per commit");
    assert!(
        rows.iter().all(|r| r.counts.churned == 0),
        "churn is attributed separately, never by the walk"
    );
}

#[test]
fn walk_buckets_and_churn_match_the_unit_model() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a.rs"), BODY).unwrap();
    commit(dir, "c1", "2026-01-01T00:00:00Z");
    std::fs::write(dir.join("a.rs"), "").unwrap();
    std::fs::write(dir.join("b.rs"), BODY).unwrap();
    commit(dir, "c2", "2026-01-02T00:00:00Z");
    let twice = format!("{BODY}\n{}", BODY.replace("compute_total", "compute_again"));
    std::fs::write(dir.join("b.rs"), &twice).unwrap();
    commit(dir, "c3", "2026-01-03T00:00:00Z");
    std::fs::write(dir.join("b.rs"), BODY).unwrap();
    commit(dir, "c4", "2026-01-08T00:00:00Z");

    let repo = open(dir.to_str().unwrap()).unwrap();
    let opts = WalkOptions::default();
    let metas = list_commits(&repo, &opts).unwrap();
    let cfg = SignalConfig {
        block_min_lines: 3,
        ..Default::default()
    };
    let (rows, _) = classify_all(&repo, &metas, &opts, &cfg, &|_| {}).unwrap();
    let mut all = Vec::new();
    let open_adds = turnover_history::attribute_churn(&mut all, rows, &[], 14 * 86_400);
    all.sort_by_key(|r| r.timestamp_unix);

    let (c1, c2, c3, c4) = (&all[0], &all[1], &all[2], &all[3]);
    assert_eq!(c1.counts.added, 5);
    assert_eq!(c1.counts.moved, 0);
    assert_eq!(c2.counts.added, 5);
    assert_eq!(c2.counts.deleted, 5);
    assert_eq!(c2.counts.moved, 4, "the function moved file: refactoring");
    assert_eq!(c3.counts.added, 5);
    assert_eq!(
        c3.counts.copy_pasted, 3,
        "three eligible inner lines pasted"
    );
    assert!(c3.counts.dup_block >= 3);
    assert_eq!(
        c3.counts.churned, 4,
        "all four eligible lines of the pasted copy were deleted five days later"
    );
    assert_eq!(c4.counts.deleted, 5);
    assert_eq!(c4.counts.added, 0);
    // c4 is 7 days after c1's remaining additions in b.rs (from c2): those are still open.
    assert!(
        open_adds.iter().all(|p| p.sha == c2.sha),
        "only c2's un-deleted adds remain pending"
    );

    // Hidden tips restrict the walk: HEAD minus c2's ancestry is c3 + c4.
    let hidden = WalkOptions {
        hidden: vec!["HEAD~2".to_string()],
        ..WalkOptions::default()
    };
    assert_eq!(list_commits(&repo, &hidden).unwrap().len(), 2);
    // Lenient mode skips an unresolvable hidden rev instead of failing.
    let lenient = WalkOptions {
        hidden: vec!["nope".to_string()],
        lenient_hidden: true,
        ..WalkOptions::default()
    };
    assert_eq!(list_commits(&repo, &lenient).unwrap().len(), 4);
    let strict = WalkOptions {
        hidden: vec!["nope".to_string()],
        ..WalkOptions::default()
    };
    assert!(list_commits(&repo, &strict).is_err());
}
