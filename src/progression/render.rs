// SPDX-License-Identifier: Apache-2.0
//! Renderers over a [`ProgressionSnapshot`]: the committed SVG, the README
//! block, and the terminal summary.
//!
//! Everything here is a pure function of the snapshot, and the snapshot carries
//! no wall clock into the drawing. That is deliberate: the SVG is a committed
//! file refreshed by CI, so anything varying run-to-run turns every push into a
//! diff and makes `progression --check` fail on a repository nobody touched.
//!
//! The SVG ships its own palette in a `<style>` block with a
//! `prefers-color-scheme` override, because a README image is rendered inside
//! an `<img>` and inherits nothing from the page around it. Light is the
//! fallback for renderers that do not evaluate the media query at all.

use std::fmt::Write as _;

use super::resolve::{NodeProgress, NodeState, ProgressionSnapshot};

/// Opening marker of the generated README block.
pub const MARKER_START: &str = "<!-- mergestro:progression:start -->";
/// Closing marker of the generated README block.
pub const MARKER_END: &str = "<!-- mergestro:progression:end -->";

const NODE_W: u32 = 208;
const NODE_H: u32 = 78;
const COL_GAP: u32 = 56;
const ROW_GAP: u32 = 18;
const PAD: u32 = 24;
const HEADER_H: u32 = 96;
const LEGEND_H: u32 = 38;

fn col_x(col: u32) -> u32 {
    PAD + col * (NODE_W + COL_GAP)
}

fn row_y(row: u32) -> u32 {
    HEADER_H + row * (NODE_H + ROW_GAP)
}

/// Total drawing size for a snapshot.
pub fn canvas_size(snap: &ProgressionSnapshot) -> (u32, u32) {
    let cols = snap.columns().max(1);
    let rows = snap.rows().max(1);
    let w = PAD * 2 + cols * NODE_W + cols.saturating_sub(1) * COL_GAP;
    let h = HEADER_H + rows * NODE_H + rows.saturating_sub(1) * ROW_GAP + LEGEND_H + PAD;
    (w, h)
}

/// XML text escaping. Attribute and text content share one escaper because the
/// values here (titles, summaries) go in both places.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Clip a label to roughly `max_chars`, with an ellipsis. SVG has no text
/// overflow, so the alternative is a title running out of its own box.
fn clip(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    while out.ends_with(' ') {
        out.pop();
    }
    out.push('…');
    out
}

/// `1 commit` / `2 commits`. A readout that says "1 PRs" reads as a bug in the
/// tool rather than a count of one.
fn plural(n: u32, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {word}s")
    }
}

fn state_class(state: NodeState) -> &'static str {
    match state {
        NodeState::Done => "done",
        NodeState::InProgress => "wip",
        NodeState::Available => "ready",
        NodeState::Locked => "locked",
    }
}

/// The committed SVG.
pub fn render_svg(snap: &ProgressionSnapshot) -> String {
    let (w, h) = canvas_size(snap);
    let mut s = String::with_capacity(4096 + snap.nodes.len() * 700);

    let alt = format!(
        "{} — {} of {} milestones done, level {}",
        snap.title, snap.totals.nodes_done, snap.totals.nodes_total, snap.totals.level
    );
    let _ = write!(
        s,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" role="img" aria-label="{}">
<title>{}</title>
<style>
  :root {{
    --paper: #FFF8EC; --ink: #111111; --muted: #6B6257; --line: #E3D8C6;
    --card: #FFFFFF; --done: #1F9D55; --wip: #E08A1E; --ready: #2A7FD4; --locked: #A99E8F;
    --track: #EDE3D3;
  }}
  @media (prefers-color-scheme: dark) {{
    :root {{
      --paper: #171512; --ink: #F3EDE3; --muted: #9C9285; --line: #332E27;
      --card: #201D19; --done: #3DD68C; --wip: #F0B44B; --ready: #5BA9F5; --locked: #6B6257;
      --track: #2B2621;
    }}
  }}
  text {{ font-family: ui-sans-serif, -apple-system, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; fill: var(--ink); }}
  .mono {{ font-family: ui-monospace, SFMono-Regular, "JetBrains Mono", Menlo, Consolas, monospace; }}
  .muted {{ fill: var(--muted); }}
  .card {{ fill: var(--card); stroke: var(--line); }}
  .edge {{ fill: none; stroke: var(--line); stroke-width: 2; }}
  .edge.met {{ stroke: var(--done); stroke-opacity: 0.55; }}
  .done .accent {{ fill: var(--done); }} .done .bar {{ fill: var(--done); }} .done .card {{ stroke: var(--done); }}
  .wip .accent {{ fill: var(--wip); }} .wip .bar {{ fill: var(--wip); }} .wip .card {{ stroke: var(--wip); }}
  .ready .accent {{ fill: var(--ready); }} .ready .bar {{ fill: var(--ready); }}
  .locked .accent {{ fill: var(--locked); }} .locked .bar {{ fill: var(--locked); }}
  .locked .title {{ fill: var(--muted); }}
  .track {{ fill: var(--track); }}
</style>
<rect width="{w}" height="{h}" fill="var(--paper)"/>
"#,
        xml_escape(&alt),
        xml_escape(&alt)
    );

    render_header(&mut s, snap, w);

    // Edges first so node cards sit on top of them.
    for node in &snap.nodes {
        for req in &node.requires {
            let Some(from) = snap.node(req) else { continue };
            let met = from.state == NodeState::Done;
            let x1 = col_x(from.col) + NODE_W;
            let y1 = row_y(from.row) + NODE_H / 2;
            let x2 = col_x(node.col);
            let y2 = row_y(node.row) + NODE_H / 2;
            let mid = (x1 + x2) / 2;
            let _ = writeln!(
                s,
                r#"<path class="edge{}" d="M{x1} {y1} C{mid} {y1} {mid} {y2} {x2} {y2}"/>"#,
                if met { " met" } else { "" }
            );
        }
    }

    for node in &snap.nodes {
        render_node(&mut s, node);
    }

    render_legend(&mut s, snap, h);
    s.push_str("</svg>\n");
    s
}

fn render_header(s: &mut String, snap: &ProgressionSnapshot, w: u32) {
    let t = &snap.totals;
    let pct = t.pct_bp / 100;
    let bar_w = w.saturating_sub(PAD * 2);
    let filled = (bar_w as u64 * t.pct_bp as u64 / 10_000) as u32;
    let season = snap
        .season
        .as_deref()
        .map(|x| format!(" · {x}"))
        .unwrap_or_default();
    let next = match t.next_level_xp {
        Some(n) => format!("{} / {n} XP to level {}", t.xp_earned, t.level + 1),
        None => format!("{} XP · max level", t.xp_earned),
    };
    let _ = write!(
        s,
        r#"<text x="{x}" y="36" font-size="19" font-weight="650">{title}</text>
<text class="muted" x="{x}" y="56" font-size="12">Level {level}{season} · {done}/{total} milestones · {parts_done}/{parts} parts · {commits} · {prs}</text>
<rect class="track" x="{x}" y="66" width="{bar_w}" height="8" rx="4"/>
<rect class="bar" x="{x}" y="66" width="{filled}" height="8" rx="4" fill="var(--done)"/>
<text class="muted mono" x="{x}" y="88" font-size="11">{pct}% · {next}</text>
"#,
        x = PAD,
        title = xml_escape(&snap.title),
        level = t.level,
        season = xml_escape(&season),
        done = t.nodes_done,
        total = t.nodes_total,
        parts_done = t.parts_done,
        parts = t.parts_total,
        commits = plural(t.commits, "commit"),
        prs = plural(t.prs, "PR"),
        next = xml_escape(&next),
    );
}

fn render_node(s: &mut String, node: &NodeProgress) {
    let x = col_x(node.col);
    let y = row_y(node.row);
    let class = state_class(node.state);
    let parts_done = node.parts.iter().filter(|p| p.done).count();
    let meta = if node.parts.is_empty() {
        format!("{} · marker", node.state.label())
    } else {
        format!(
            "{}/{} parts · {} XP · {}{}",
            parts_done,
            node.parts.len(),
            node.xp_earned,
            plural(node.commits, "commit"),
            if node.prs.is_empty() {
                String::new()
            } else {
                format!(" · {}", plural(node.prs.len() as u32, "PR"))
            }
        )
    };
    let track_w = NODE_W - 32;
    let filled = (track_w as u64 * node.pct_bp as u64 / 10_000) as u32;
    let _ = write!(
        s,
        r#"<g class="{class}" id="node-{id}">
  <rect class="card" x="{x}" y="{y}" width="{w}" height="{h}" rx="10" stroke-width="1.5"/>
  <rect class="accent" x="{x}" y="{ay}" width="4" height="{ah}" rx="2"/>
  <text class="title" x="{tx}" y="{ty}" font-size="13.5" font-weight="600">{title}</text>
  <text class="muted" x="{tx}" y="{my}" font-size="10.5">{meta}</text>
  <rect class="track" x="{tx}" y="{by}" width="{track_w}" height="6" rx="3"/>
  <rect class="bar" x="{tx}" y="{by}" width="{filled}" height="6" rx="3"/>
</g>
"#,
        class = class,
        id = xml_escape(&node.id),
        x = x,
        y = y,
        w = NODE_W,
        h = NODE_H,
        ay = y + 12,
        ah = NODE_H - 24,
        tx = x + 16,
        ty = y + 26,
        my = y + 44,
        by = y + 56,
        title = xml_escape(&clip(&node.title, 26)),
        meta = xml_escape(&meta),
        track_w = track_w,
        filled = filled,
    );
}

fn render_legend(s: &mut String, snap: &ProgressionSnapshot, h: u32) {
    let y = h - PAD - 8;
    let items = [
        (NodeState::Done, snap.totals.nodes_done),
        (NodeState::InProgress, snap.totals.nodes_in_progress),
        (NodeState::Available, snap.totals.nodes_available),
        (NodeState::Locked, snap.totals.nodes_locked),
    ];
    let mut x = PAD;
    for (state, n) in items {
        let label = format!("{} {}", n, state.label());
        let _ = writeln!(
            s,
            r#"<g class="{cls}"><circle class="accent" cx="{cx}" cy="{cy}" r="4"/><text class="muted" x="{tx}" y="{ty}" font-size="11">{label}</text></g>"#,
            cls = state_class(state),
            cx = x + 4,
            cy = y - 4,
            tx = x + 14,
            ty = y,
            label = xml_escape(&label),
        );
        // ~6.2px per character at 11px, plus the swatch and the gap after it.
        x += 14 + (label.chars().count() as u32 * 6) + 22;
    }
}

/// The block injected between the README markers.
///
/// `svg_href` is the path the README should link the image at — relative to the
/// README, which is why the caller computes it rather than this function
/// guessing from the output path.
pub fn render_markdown(snap: &ProgressionSnapshot, svg_href: Option<&str>) -> String {
    let t = &snap.totals;
    let mut s = String::new();
    s.push_str("<!-- Generated by `slop-gate progression`. Edit the spec, not this block. -->\n");
    if let Some(href) = svg_href {
        let (w, _) = canvas_size(snap);
        let alt = format!(
            "{} — {} of {} milestones done",
            snap.title, t.nodes_done, t.nodes_total
        );
        let _ = write!(
            s,
            "<img src=\"{}\" alt=\"{}\" width=\"{}\">\n\n",
            href,
            xml_escape(&alt),
            w.min(900)
        );
    }
    let season = snap
        .season
        .as_deref()
        .map(|x| format!(" · {x}"))
        .unwrap_or_default();
    let _ = writeln!(
        s,
        "**Level {}{} — {}% · {}/{} XP · {}/{} milestones**",
        t.level,
        season,
        t.pct_bp / 100,
        t.xp_earned,
        t.xp_total,
        t.nodes_done,
        t.nodes_total
    );
    s.push('\n');
    s.push_str("| Milestone | State | Parts | Evidence |\n| --- | --- | --- | --- |\n");
    for node in &snap.nodes {
        let done = node.parts.iter().filter(|p| p.done).count();
        let evidence = if node.commits == 0 {
            "—".to_string()
        } else if node.prs.is_empty() {
            plural(node.commits, "commit")
        } else {
            format!(
                "{} · {}",
                plural(node.commits, "commit"),
                plural(node.prs.len() as u32, "PR")
            )
        };
        let _ = writeln!(
            s,
            "| {} | {} | {}/{} | {} |",
            md_escape(&node.title),
            node.state.label(),
            done,
            node.parts.len(),
            evidence
        );
    }
    s
}

/// Escape the characters that would break a Markdown table cell.
fn md_escape(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

/// A terminal summary — what the subcommand prints when it is not writing files.
pub fn render_text(snap: &ProgressionSnapshot) -> String {
    let t = &snap.totals;
    let mut s = String::new();
    let _ = writeln!(s, "{}", snap.title);
    let _ = writeln!(
        s,
        "  level {} · {}% · {}/{} XP · {}/{} milestones · {}/{} parts",
        t.level,
        t.pct_bp / 100,
        t.xp_earned,
        t.xp_total,
        t.nodes_done,
        t.nodes_total,
        t.parts_done,
        t.parts_total
    );
    let _ = writeln!(
        s,
        "  evidence: {} · {} · head {}",
        plural(t.commits, "commit"),
        plural(t.prs, "PR"),
        &snap.head_sha[..snap.head_sha.len().min(7)]
    );
    s.push('\n');
    for node in &snap.nodes {
        let done = node.parts.iter().filter(|p| p.done).count();
        let _ = writeln!(
            s,
            "  [{:^11}] {:<34} {}/{} parts  {} XP",
            node.state.label(),
            clip(&node.title, 34),
            done,
            node.parts.len(),
            node.xp_earned
        );
        for part in &node.parts {
            let mark = if part.done { "x" } else { " " };
            let need = if part.manual {
                "manual".to_string()
            } else if part.need_prs > 0 && part.need_commits > 0 {
                format!(
                    "{}/{} commits, {}/{} PRs",
                    part.have_commits, part.need_commits, part.have_prs, part.need_prs
                )
            } else if part.need_prs > 0 {
                format!("{}/{} PRs", part.have_prs, part.need_prs)
            } else {
                format!("{}/{} commits", part.have_commits, part.need_commits)
            };
            let _ = writeln!(s, "      [{mark}] {:<40} {need}", clip(&part.title, 40));
        }
    }
    s
}

/// Replace the generated block in a README, or report that the markers are missing.
///
/// Injection rather than "rewrite the file": the README is a human document
/// with the block somewhere in the middle, and the markers are the contract
/// that says which part CI owns.
pub fn inject_readme(readme: &str, block: &str) -> anyhow::Result<String> {
    let start = readme.find(MARKER_START).ok_or_else(|| {
        anyhow::anyhow!(
            "README has no `{MARKER_START}` marker — add the marker pair where the tree should go"
        )
    })?;
    let end = readme.find(MARKER_END).ok_or_else(|| {
        anyhow::anyhow!("README has `{MARKER_START}` but no closing `{MARKER_END}` marker")
    })?;
    if end < start {
        anyhow::bail!(
            "README markers are in the wrong order (`{MARKER_END}` before `{MARKER_START}`)"
        );
    }
    let mut out = String::with_capacity(readme.len() + block.len());
    out.push_str(&readme[..start + MARKER_START.len()]);
    out.push('\n');
    out.push_str(block.trim_end());
    out.push('\n');
    out.push_str(&readme[end..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progression::resolve::{PartProgress, ProgressionTotals};

    fn node(id: &str, state: NodeState, col: u32, row: u32) -> NodeProgress {
        NodeProgress {
            id: id.into(),
            title: format!("Node {id}"),
            summary: None,
            requires: Vec::new(),
            state,
            xp_earned: 10,
            xp_total: 20,
            pct_bp: 5_000,
            parts: vec![PartProgress {
                id: "p".into(),
                title: "part".into(),
                done: true,
                xp: 10,
                manual: false,
                have_commits: 2,
                need_commits: 1,
                have_prs: 1,
                need_prs: 0,
                evidence: Vec::new(),
            }],
            commits: 2,
            prs: vec![7],
            first_activity_unix: Some(1),
            last_activity_unix: Some(2),
            col,
            row,
        }
    }

    fn snap() -> ProgressionSnapshot {
        ProgressionSnapshot {
            version: 1,
            title: "Plan & <plan>".into(),
            season: Some("2026 H1".into()),
            generated_at_unix: 1_700_000_000,
            head_sha: "0123456789abcdef".into(),
            since_days: None,
            totals: ProgressionTotals {
                xp_earned: 20,
                xp_total: 40,
                pct_bp: 5_000,
                nodes_total: 2,
                nodes_done: 1,
                nodes_in_progress: 1,
                parts_total: 2,
                parts_done: 2,
                commits: 4,
                prs: 1,
                level: 2,
                level_floor_xp: 0,
                next_level_xp: Some(50),
                ..Default::default()
            },
            nodes: vec![node("a", NodeState::Done, 0, 0), {
                let mut n = node("b", NodeState::InProgress, 1, 0);
                n.requires = vec!["a".into()];
                n
            }],
        }
    }

    #[test]
    fn the_svg_escapes_markup_in_authored_text() {
        let s = render_svg(&snap());
        assert!(s.contains("Plan &amp; &lt;plan&gt;"), "{s}");
        // The raw form must appear nowhere — a title is author input, and the
        // SVG is committed into a README that GitHub serves.
        assert!(!s.contains("<plan>"));
        assert!(s.starts_with("<svg xmlns="));
        assert!(s.trim_end().ends_with("</svg>"));
    }

    #[test]
    fn the_svg_carries_no_wall_clock() {
        // `generated_at_unix` is in the snapshot on purpose and must not reach
        // the drawing, or every CI refresh is a diff and `--check` never passes.
        let s = render_svg(&snap());
        assert!(!s.contains("1700000000"));
        let again = render_svg(&snap());
        assert_eq!(s, again);
    }

    #[test]
    fn an_edge_is_drawn_for_every_requirement() {
        let s = render_svg(&snap());
        assert_eq!(s.matches("class=\"edge").count(), 1);
        // The requirement is done, so the edge is the "met" variant.
        assert!(s.contains("class=\"edge met\""));
    }

    #[test]
    fn canvas_grows_with_the_tree_and_never_underflows_on_one_node() {
        let mut one = snap();
        one.nodes.truncate(1);
        let (w, h) = canvas_size(&one);
        assert_eq!(w, PAD * 2 + NODE_W);
        assert!(h > HEADER_H);

        let (w2, _) = canvas_size(&snap());
        assert_eq!(w2, PAD * 2 + 2 * NODE_W + COL_GAP);
    }

    #[test]
    fn readme_injection_replaces_only_the_block() {
        let readme = format!("# Title\n\nbefore\n\n{MARKER_START}\nstale\n{MARKER_END}\n\nafter\n");
        let out = inject_readme(&readme, "fresh").unwrap();
        assert!(out.contains("before"));
        assert!(out.contains("after"));
        assert!(out.contains("fresh"));
        assert!(!out.contains("stale"));

        // Injecting twice is a fixed point — that is what makes the CI step
        // idempotent and the `--check` comparison meaningful.
        let twice = inject_readme(&out, "fresh").unwrap();
        assert_eq!(out, twice);
    }

    #[test]
    fn missing_or_reversed_markers_are_an_error_not_a_silent_append() {
        let err = inject_readme("# Title\n", "x").unwrap_err().to_string();
        assert!(
            err.contains("no `<!-- mergestro:progression:start -->` marker"),
            "{err}"
        );

        let reversed = format!("{MARKER_END}\n{MARKER_START}\n");
        assert!(inject_readme(&reversed, "x")
            .unwrap_err()
            .to_string()
            .contains("wrong order"));
    }

    #[test]
    fn counts_of_one_are_singular() {
        // "1 PRs" in a generated README reads as a broken tool.
        assert_eq!(plural(1, "commit"), "1 commit");
        assert_eq!(plural(0, "commit"), "0 commits");
        assert_eq!(plural(2, "PR"), "2 PRs");

        let md = render_markdown(&snap(), None);
        assert!(md.contains("2 commits · 1 PR |"), "{md}");
        assert!(!md.contains("1 PRs"));
    }

    #[test]
    fn markdown_escapes_pipes_so_the_table_survives() {
        let mut s = snap();
        s.nodes[0].title = "a | b".into();
        let md = render_markdown(&s, Some("docs/progression.svg"));
        assert!(md.contains("a \\| b"), "{md}");
        assert!(md.contains("<img src=\"docs/progression.svg\""));
    }

    #[test]
    fn text_summary_lists_every_node_and_part() {
        let out = render_text(&snap());
        assert!(out.contains("Node a"));
        assert!(out.contains("Node b"));
        assert_eq!(out.matches("[x]").count(), 2);
    }
}
