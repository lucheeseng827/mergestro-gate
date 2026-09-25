//! The binary, end to end: baseline -> gate -> emit, on a synthetic repository whose second
//! half is pasted code. The gate must fail on drift in blocking mode, pass in advisory mode,
//! refuse to run without a baseline, and emit a Mergestro record with the right shape.

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str], date: &str) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
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
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn turnover(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_turnover"))
        .args(args)
        .current_dir(dir)
        .env_remove("GITHUB_REPOSITORY")
        .env_remove("GITHUB_RUN_ID")
        .env_remove("GITHUB_REF_NAME")
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Honest code: each function has a body of four statements drawn from a pool of pairwise
/// shape-distinct lines (each one a longer arithmetic chain than the last), rotated by `i`, so
/// no four consecutive significant lines repeat anywhere in the file even after identifiers
/// and literals are abstracted. `func` below is the opposite: same shape, different names.
fn honest(i: usize) -> String {
    let stmt = |k: usize| {
        let mut line = format!("    let acc_{k} = base_{k}");
        for j in 0..=k {
            line.push_str(&format!(" + items[{j}].price"));
        }
        line.push_str(";\n");
        line
    };
    let mut body = format!("fn honest_{i}(items: &[Item], base_0: u64) -> u64 {{\n");
    for k in i..i + 4 {
        body.push_str(&stmt(k));
    }
    body.push_str(&format!("    acc_{}\n}}\n", i + 3));
    body
}

fn func(name: &str, i: usize) -> String {
    format!(
        "fn {name}_{i}(items: &[Item]) -> u64 {{\n    let mut total_{i} = 0;\n    for item in items.iter().filter(|it| it.kind == {i}) {{\n        total_{i} += item.price * item.qty + {i};\n    }}\n    total_{i}\n}}\n"
    )
}

#[test]
fn baseline_then_gate_fails_on_paste_drift() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"], "2026-01-01T00:00:00Z");
    // Policy: small window so the tests need few lines; drift limit 5 points.
    std::fs::write(
        dir.join("turnover.toml"),
        "[gate]\nwindow_days = 10\nmin_added_lines = 20\n[signals]\nblock_min_lines = 4\n",
    )
    .unwrap();

    // Twenty days of distinct, honest code: one unique function per day.
    for day in 1..=20 {
        let mut body = String::new();
        for i in 0..day {
            body.push_str(&honest(i));
        }
        std::fs::write(dir.join("lib.rs"), body).unwrap();
        git(dir, &["add", "-A"], "x");
        git(
            dir,
            &["commit", "-q", "-m", &format!("day {day}")],
            &format!("2026-01-{day:02}T00:00:00Z"),
        );
    }

    let (code, _, err) = turnover(dir, &["gate"]);
    assert_eq!(code, 2, "no baseline yet must be an error: {err}");
    assert!(err.contains("run `turnover baseline` first"));

    let (code, _, err) = turnover(dir, &["baseline"]);
    assert_eq!(code, 0, "{err}");
    assert!(dir.join(".turnover/baseline.json").exists());
    assert!(err.contains("20 new commits classified"));

    // Honest history: the trailing window matches the baseline -> pass.
    let (code, out, err) = turnover(dir, &["gate"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("verdict: PASS"), "{out}");

    // Now ten days of pasting the same function over and over.
    let mut body = std::fs::read_to_string(dir.join("lib.rs")).unwrap();
    for day in 21..=30 {
        for _ in 0..3 {
            body.push_str(
                &func("pasted", 7).replace("pasted_7", &format!("pasted_{day}_{}", body.len())),
            );
        }
        std::fs::write(dir.join("lib.rs"), &body).unwrap();
        git(dir, &["add", "-A"], "x");
        git(
            dir,
            &[
                "commit",
                "-q",
                "-m",
                &format!("day {day}\n\nCo-Authored-By: Claude <noreply@anthropic.com>"),
            ],
            &format!("2026-01-{day:02}T00:00:00Z"),
        );
    }

    let (code, out, err) = turnover(
        dir,
        &[
            "gate",
            "--json",
            "report.json",
            "--emit",
            "records.jsonl",
            "--repo-name",
            "acme/api",
            "--run-id",
            "r1",
        ],
    );
    assert_eq!(code, 1, "blocking drift must fail: {out}{err}");
    assert!(out.contains("verdict: FAIL (blocking)"), "{out}");
    assert!(out.contains("copy_paste rose"), "{out}");
    assert!(
        out.contains("walked 10 commits"),
        "gate refreshes the baseline in memory: {out}{err}"
    );

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["verdict"]["status"], "fail");
    assert_eq!(report["repo"], "acme/api");
    assert!(
        report["window"]["ratios"]["copy_paste"].as_f64().unwrap()
            > report["baseline"]["ratios"]["copy_paste"].as_f64().unwrap()
    );

    let rec: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(dir.join("records.jsonl"))
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(rec["record_type"], "turnover");
    assert_eq!(rec["repo"], "acme/api");
    assert_eq!(rec["run_id"], "r1");
    assert_eq!(rec["verdict"], "fail");
    assert_eq!(rec["mode"], "blocking");
    assert_eq!(rec["window_days"], 10);
    assert_eq!(rec["failed_checks"][0], "copy_paste");
    assert!(rec["head_sha"].as_str().unwrap().len() == 40);
    assert!(rec["ratios"]["copy_paste"].as_f64().unwrap() > 0.5);
    assert_eq!(rec["origins"]["ai"]["commits"], 10, "{rec}");
    assert!(
        rec["origins"].get("human").is_none(),
        "the window is entirely AI-coauthored: {rec}"
    );
    assert!(
        out.contains("AI-coauthored commits (10 of 10) added 100.0% of the lines"),
        "{out}"
    );
    assert!(
        out.contains("where: lib.rs @"),
        "the offenders list names the file: {out}"
    );
    assert!(
        report["offenders"][0]["path"] == "lib.rs",
        "{}",
        report["offenders"]
    );
    assert!(
        report["offenders"][0]["dup_blocks"][0]["other_path"] == "lib.rs",
        "{}",
        report["offenders"]
    );

    // The Markdown section for a PR comment carries the evidence.
    let (_, md, _) = turnover(dir, &["gate", "--markdown"]);
    assert!(
        md.contains("**Where it comes from** (worst first):"),
        "{md}"
    );
    assert!(md.contains("- `lib.rs` @"), "{md}");
    assert!(md.contains("duplicate `lib.rs:"), "{md}");

    // Advisory mode reports the same failure but exits 0.
    let (code, out, _) = turnover(dir, &["gate", "--advisory"]);
    assert_eq!(code, 0);
    assert!(out.contains("verdict: FAIL (advisory)"), "{out}");

    // A PR-scoped gate: the pasted branch over the honest history.
    let (code, out, _) = turnover(dir, &["gate", "--base-ref", "HEAD~10"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("commits in HEAD not in HEAD~10 (10 commits)"),
        "{out}"
    );

    // Report and explain run over the same baseline.
    let (code, out, _) = turnover(dir, &["report", "--bucket-days", "10"]);
    assert_eq!(code, 0);
    assert!(out.lines().count() >= 4, "{out}");
    let (code, out, _) = turnover(dir, &["explain", "HEAD"]);
    assert_eq!(code, 0);
    assert!(out.contains("lib.rs [rust]"), "{out}");
    assert!(out.contains("pasted lines:"), "{out}");
}

#[test]
fn tiny_windows_are_insufficient_samples() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"], "2026-01-01T00:00:00Z");
    std::fs::write(dir.join("a.rs"), func("only", 1)).unwrap();
    git(dir, &["add", "-A"], "x");
    git(dir, &["commit", "-q", "-m", "one"], "2026-01-01T00:00:00Z");
    assert_eq!(turnover(dir, &["baseline"]).0, 0);
    let (code, out, _) = turnover(dir, &["gate"]);
    assert_eq!(code, 0);
    assert!(out.contains("INSUFFICIENT SAMPLE"), "{out}");
}
