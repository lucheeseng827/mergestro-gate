//! Reports. The text form is what the job log shows; the Markdown form is the section the
//! Mergestro gate's PR comment carries (and what `turnover gate --markdown` prints).

use turnover_core::policy::{CheckKind, Mode, Status, Verdict};
use turnover_core::window::{Aggregate, Ratios};
use turnover_history::Stats;

use crate::run::{ymd, GateOutcome};

pub fn pct(x: Option<f64>) -> String {
    match x {
        Some(v) => format!("{:>5.1}%", v * 100.0),
        None => "    —".to_string(),
    }
}

fn pct_plain(x: Option<f64>) -> String {
    match x {
        Some(v) => format!("{:.1}%", v * 100.0),
        None => "—".to_string(),
    }
}

pub fn ratio_line(a: &Aggregate) -> String {
    format!(
        "copy/paste {} · dup block {} · refactor {} · churn {} over {} added lines in {} commits",
        pct(a.ratios.copy_paste).trim(),
        pct(a.ratios.dup_block).trim(),
        pct(a.ratios.refactor).trim(),
        pct(a.ratios.churn).trim(),
        a.counts.added,
        a.commits
    )
}

pub fn stats_line(s: &Stats) -> Option<String> {
    if s.commits == 0 {
        return None;
    }
    Some(format!(
        "walked {} commits ({} merges skipped), {} files: {} ignored path, {} unsupported, {} binary, {} too large; lines masked by grammar {} / heuristic {}",
        s.commits, s.merges_skipped, s.files, s.files_ignored_path, s.files_unsupported, s.files_binary, s.files_too_large, s.lines_parsed, s.lines_heuristic
    ))
}

/// The four signal rows: (label, policy key, window ratio, baseline ratio).
fn rows(w: &Ratios, b: &Ratios) -> [(&'static str, &'static str, Option<f64>, Option<f64>); 4] {
    [
        ("copy/paste", "copy_paste", w.copy_paste, b.copy_paste),
        ("dup block", "dup_block", w.dup_block, b.dup_block),
        ("refactor", "refactor", w.refactor, b.refactor),
        ("churn", "churn", w.churn, b.churn),
    ]
}

fn checks_for<'a>(
    v: &'a Verdict,
    key: &str,
) -> impl Iterator<Item = &'a turnover_core::policy::Check> {
    let key = key.to_string();
    v.checks.iter().filter(move |c| c.signal == key)
}

fn check_word(kind: CheckKind) -> &'static str {
    match kind {
        CheckKind::Absolute => "cap",
        CheckKind::Drift => "drift",
    }
}

/// The job-log report.
pub fn text(o: &GateOutcome) -> String {
    let mut out = String::new();
    let (w, b, v) = (&o.window, &o.baseline, &o.verdict);
    out.push_str(&format!(
        "turnover: window = {}: {} commits by {} authors, {} significant lines added\n",
        o.scope, w.commits, w.authors, w.counts.added
    ));
    if o.has_baseline {
        out.push_str(&format!(
            "turnover: baseline = {} commits, {} significant lines added ({} → {})\n",
            b.commits,
            b.counts.added,
            ymd(b.from_unix),
            ymd(b.to_unix - 1)
        ));
    } else {
        out.push_str("turnover: baseline = none yet (only absolute caps apply)\n");
    }
    out.push_str(&format!(
        "  {:<11} {:>7} {:>9}   checks\n",
        "signal", "window", "baseline"
    ));
    for (name, key, wr, br) in rows(&w.ratios, &b.ratios) {
        let checks: Vec<String> = checks_for(v, key)
            .map(|c| {
                format!(
                    "{} {} {}",
                    check_word(c.kind),
                    pct(Some(c.limit)).trim(),
                    if c.passed { "ok" } else { "FAIL" }
                )
            })
            .collect();
        out.push_str(&format!(
            "  {:<11} {:>7} {:>9}   {}\n",
            name,
            pct(wr),
            if o.has_baseline {
                pct(br)
            } else {
                "    —".to_string()
            },
            if checks.is_empty() {
                "(no limit set)".to_string()
            } else {
                checks.join(" · ")
            }
        ));
    }
    match v.status {
        Status::Pass => out.push_str(&format!("verdict: PASS ({})\n", v.mode.as_str())),
        Status::InsufficientSample => out.push_str(&format!(
            "verdict: INSUFFICIENT SAMPLE — {} significant lines added, gate needs {} (passes)\n",
            v.window_added_lines, v.min_added_lines
        )),
        Status::Fail => {
            for c in v.failed_checks() {
                out.push_str(&format!("  ✗ {}\n", c.message));
            }
            out.push_str(&format!(
                "verdict: FAIL ({}){}\n",
                v.mode.as_str(),
                if v.mode == Mode::Blocking {
                    " — exit 1"
                } else {
                    " — advisory, exit 0"
                }
            ));
        }
    }
    if let Some(line) = origin_line(&o.window) {
        out.push_str(&format!("turnover: {line}\n"));
    }
    for f in &o.offenders {
        out.push_str(&format!(
            "  where: {} @{} — +{} lines, {} pasted, {} in duplicated blocks{}\n",
            f.path,
            f.sha,
            f.added,
            f.copy_pasted,
            f.dup_block,
            twins_text(f)
        ));
    }
    if let Some(s) = stats_line(&o.walk) {
        out.push_str(&format!("turnover: {s}\n"));
    }
    out
}

/// "AI-coauthored commits added 62% of the lines: copy/paste 18.2% vs human 6.1%".
pub fn origin_line(a: &Aggregate) -> Option<String> {
    let ai = a.ai()?;
    let share = a.ai_share()?;
    let human = a.human();
    let vs = |name: &str, f: fn(&Ratios) -> Option<f64>| match human {
        Some(h) => format!(
            "{name} {} vs human {}",
            pct_plain(f(&ai.ratios)),
            pct_plain(f(&h.ratios))
        ),
        None => format!("{name} {}", pct_plain(f(&ai.ratios))),
    };
    Some(format!(
        "AI-coauthored commits ({} of {}) added {} of the lines — {} · {} · {} · {}",
        ai.commits,
        a.commits,
        pct_plain(Some(share)),
        vs("copy/paste", |r| r.copy_paste),
        vs("dup block", |r| r.dup_block),
        vs("refactor", |r| r.refactor),
        vs("churn", |r| r.churn)
    ))
}

fn twins_text(f: &crate::run::FileOffender) -> String {
    if f.dup_blocks.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = f
        .dup_blocks
        .iter()
        .map(|b| {
            format!(
                "lines {}–{} duplicate {}:{}",
                b.line,
                b.line + b.lines - 1,
                b.other_path,
                b.other_line
            )
        })
        .collect();
    format!("; {}", parts.join("; "))
}

/// The Markdown section for a PR comment. No top-level heading: the caller owns the
/// document and places this under its own `###`.
pub fn markdown(o: &GateOutcome) -> String {
    let mut out = String::new();
    let (w, b, v) = (&o.window, &o.baseline, &o.verdict);
    let badge = match v.status {
        Status::Pass => "✅ within the drift budget",
        Status::Fail if v.mode == Mode::Blocking => "❌ drifted past the baseline",
        Status::Fail => "⚠️ drifted past the baseline (advisory)",
        Status::InsufficientSample => "ℹ️ not enough new code to judge",
    };
    out.push_str(&format!(
        "{badge} — {}: **{}** significant lines added in {} commits",
        o.scope, w.counts.added, w.commits
    ));
    if o.has_baseline {
        out.push_str(&format!(
            ", baseline {} commits ({} → {})",
            b.commits,
            ymd(b.from_unix),
            ymd(b.to_unix - 1)
        ));
    }
    out.push_str(".\n\n");
    if v.status == Status::InsufficientSample {
        out.push_str(&format!(
            "The gate needs {} significant added lines before a ratio means anything; this change has {}.\n",
            v.min_added_lines, v.window_added_lines
        ));
        return out;
    }
    out.push_str(
        "| Signal | This change | Baseline | Limit | |\n| --- | ---: | ---: | --- | --- |\n",
    );
    for (name, key, wr, br) in rows(&w.ratios, &b.ratios) {
        let checks: Vec<&turnover_core::policy::Check> = checks_for(v, key).collect();
        let limit = if checks.is_empty() {
            "no limit set".to_string()
        } else {
            checks
                .iter()
                .map(|c| format!("{} {}", check_word(c.kind), pct_plain(Some(c.limit))))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mark = if checks.is_empty() {
            ""
        } else if checks.iter().all(|c| c.passed) {
            "✅"
        } else {
            "❌"
        };
        out.push_str(&format!(
            "| {name} | {} | {} | {limit} | {mark} |\n",
            pct_plain(wr),
            if o.has_baseline {
                pct_plain(br)
            } else {
                "—".to_string()
            }
        ));
    }
    let failed: Vec<String> = v
        .failed_checks()
        .map(|c| format!("- {}", c.message))
        .collect();
    if !failed.is_empty() {
        out.push('\n');
        out.push_str(&failed.join("\n"));
        out.push('\n');
    }
    if let Some(line) = origin_line(w) {
        out.push_str(&format!("\n{line}.\n"));
    }
    if !o.offenders.is_empty() {
        out.push_str("\n**Where it comes from** (worst first):\n");
        for f in &o.offenders {
            out.push_str(&format!(
                "- `{}` @{} — +{} lines, {} pasted, {} in duplicated blocks",
                f.path, f.sha, f.added, f.copy_pasted, f.dup_block
            ));
            for b in &f.dup_blocks {
                out.push_str(&format!(
                    "; lines {}–{} duplicate `{}:{}`",
                    b.line,
                    b.line + b.lines - 1,
                    b.other_path,
                    b.other_line
                ));
            }
            if f.dup_blocks.is_empty() && !f.pasted_lines.is_empty() {
                let shown: Vec<String> = f.pasted_lines.iter().map(|n| n.to_string()).collect();
                out.push_str(&format!(" (pasted lines {})", shown.join(", ")));
            }
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use turnover_core::policy::{evaluate, Policy};
    use turnover_core::signals::Counts;

    fn agg(added: u64, pasted: u64, moved: u64) -> Aggregate {
        let counts = Counts {
            added,
            copy_pasted: pasted,
            moved,
            ..Default::default()
        };
        Aggregate {
            counts,
            ratios: Ratios::of(&counts),
            commits: 3,
            authors: 1,
            ..Default::default()
        }
    }

    fn outcome(window: Aggregate, baseline: Aggregate) -> GateOutcome {
        let policy = Policy::default();
        let verdict = evaluate(&policy, Some(&baseline), &window);
        GateOutcome {
            tool_version: "t",
            head_sha: "abc".into(),
            generated_at_unix: 0,
            scope: "commits in HEAD not in main (3 commits)".into(),
            window_days: 90,
            verdict,
            window,
            baseline,
            has_baseline: true,
            baseline_commits: 10,
            walk: Stats::default(),
            offenders: Vec::new(),
            explained_commits: 0,
            policy,
        }
    }

    #[test]
    fn markdown_marks_failed_rows_and_lists_reasons() {
        let o = outcome(agg(1000, 300, 10), agg(10_000, 800, 900));
        let md = markdown(&o);
        assert!(md.contains("❌ drifted past the baseline"), "{md}");
        assert!(
            md.contains("| copy/paste | 30.0% | 8.0% | drift 13.0% | ❌ |"),
            "{md}"
        );
        assert!(
            md.contains("- copy_paste rose to 30.0% from baseline 8.0%"),
            "{md}"
        );
        let t = text(&o);
        assert!(t.contains("verdict: FAIL (blocking) — exit 1"), "{t}");
    }

    #[test]
    fn origin_split_and_offenders_render() {
        use turnover_core::signals::DupBlock;
        use turnover_core::window::OriginAggregate;
        let mut o = outcome(agg(1000, 300, 10), agg(10_000, 800, 900));
        let ai_counts = Counts {
            added: 600,
            copy_pasted: 280,
            ..Default::default()
        };
        let human_counts = Counts {
            added: 400,
            copy_pasted: 20,
            ..Default::default()
        };
        o.window.by_origin.insert(
            "ai".into(),
            OriginAggregate {
                commits: 2,
                counts: ai_counts,
                ratios: Ratios::of(&ai_counts),
            },
        );
        o.window.by_origin.insert(
            "human".into(),
            OriginAggregate {
                commits: 1,
                counts: human_counts,
                ratios: Ratios::of(&human_counts),
            },
        );
        o.offenders.push(crate::run::FileOffender {
            path: "src/x.rs".into(),
            language: "rust".into(),
            sha: "abc1234567".into(),
            added: 120,
            copy_pasted: 40,
            dup_block: 60,
            moved: 0,
            pasted_lines: vec![10, 11],
            dup_blocks: vec![DupBlock {
                line: 100,
                lines: 12,
                other_path: "src/y.rs".into(),
                other_line: 40,
            }],
        });
        let md = markdown(&o);
        assert!(md.contains("AI-coauthored commits (2 of 3) added 60.0% of the lines — copy/paste 46.7% vs human 5.0%"), "{md}");
        assert!(
            md.contains("**Where it comes from** (worst first):"),
            "{md}"
        );
        assert!(md.contains("- `src/x.rs` @abc1234567 — +120 lines, 40 pasted, 60 in duplicated blocks; lines 100–111 duplicate `src/y.rs:40`"), "{md}");
        let t = text(&o);
        assert!(t.contains("where: src/x.rs @abc1234567"), "{t}");
    }

    #[test]
    fn insufficient_sample_explains_itself() {
        let o = outcome(agg(10, 10, 0), agg(10_000, 800, 900));
        let md = markdown(&o);
        assert!(md.contains("not enough new code to judge"), "{md}");
        assert!(md.contains("needs 200 significant added lines"), "{md}");
    }
}
