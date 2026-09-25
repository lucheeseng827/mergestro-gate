# Progression spec — format digest

*Paste this to an assistant along with one of the prompts in this directory. It is the whole
format in one page.*

A progression spec is a hand-authored plan. The repository never writes it; the repository only
**closes** what the plan declared. Every part carries an evidence rule that commits and merged
PRs satisfy, so the plan is checkable rather than aspirational.

```yaml
version: 1                          # only 1 exists
title: Acme API — 2026 H1           # the canvas heading (required)
season: 2026 H1                     # optional free-form period label
pr_pattern: '\(#(\d+)\)'            # regex with ONE capture group: the PR number in a subject
levels: [0, 50, 150, 350, 700]      # optional, strictly ascending cumulative XP thresholds
since_days: 14                      # optional; makes the tree a SLIDING window (see below)

nodes:
  - id: ingest                      # stable, unique; used by `requires`, SVG anchors, the canvas
    title: Ingest pipeline
    summary: One line, shown in the console detail pane.   # optional
    requires: [foundations]         # milestones that must be `done` before this leaves `locked`
    paths: ["src/ingest/**"]        # optional: scopes every part below that does not narrow
    parts:
      - id: validate
        title: Reject a malformed batch at the door
        xp: 20                      # default 10
        evidence: { paths: ["src/ingest/validate.rs"] }
```

## Evidence

Every declared condition is ANDed. `paths` and `subject` **select** commits; `commits` and
`prs` are **thresholds** over that selection.

| Key | Meaning |
| --- | --- |
| `paths` | Repo-relative globs. `**` crosses directories, `*` does not, `?` is one non-`/` character. A pattern with no wildcard also covers everything beneath it, so `docs` matches `docs/CONFIG.md`. A part with no `paths` of its own inherits the node's. |
| `subject` | Regex over the commit subject **and body**, case-insensitive. |
| `authors` | Substring match on the author. Empty means anyone. |
| `commits` | How many matching commits close the part. Defaults to 1 when a selector is set. |
| `prs` | How many *distinct* PRs among the matching commits. Pair with `commits: 0` for a PR-only rule. |
| `manual` | `true` **closes the part by hand.** For work no commit can prove. Never a placeholder. |

## Rules the validator enforces — a spec breaking any of these will not load

1. Node ids unique; part ids unique within their node.
2. Every `requires` names an existing node. No self-reference, **no cycles**.
3. Every part declares evidence. An empty `evidence: {}` is rejected — that is a plan item
   nobody wired up, and reporting it as done is the failure this format exists to avoid.
4. A part whose thresholds are all zero is rejected: it could never close. `commits: 0` is
   legal **only** paired with `prs:`.
5. `levels` strictly ascending; `pr_pattern` and every `subject` a valid regex; every glob
   compilable.

## Rules the validator cannot enforce — these are the ones that ruin a plan

1. **A glob that matches nothing renders exactly like a milestone nobody started.** Never write
   a path you have not seen. Check one with `git log --oneline -- <dir> | head`: silence means
   no commit ever touched it.
   *Exception, and an important one:* a path that does not exist **yet** is correct for planned
   work. The rule is directional — an unmatched glob is fatal for work you believe is finished,
   and expected for work you are planning.
2. **A milestone that was already finished when it was written** makes the tree read 100% on
   day one and then stop moving. Compress finished work into one or two low-XP baseline
   milestones; spend the XP on what is open.
3. **A milestone nothing can falsify** ("improve performance", "better tests") cannot close and
   cannot be argued with. Every part names a file, a directory, or a subject convention the
   team actually writes.

## States and scoring

| State | Meaning |
| --- | --- |
| `locked` | Something it requires is not done |
| `available` | Unblocked, no evidence yet |
| `in_progress` | Some parts closed |
| `done` | Every part closed **and** every requirement done |

A node's XP is the sum of its parts; earned XP is the sum of the closed ones. Level comes from
`levels`, or eight built-in bands.

## Two behaviours worth knowing before you author

* **Merge commits are skipped.** A merge's first-parent diff re-attributes every file on the
  branch to one commit, which would close a milestone on the strength of a merge that "touched"
  a thousand paths. The branch's own commits are in the walk anyway.
* **`since_days` slides.** It is measured from *now* at every resolve, so a milestone closed by
  commits that later fall outside the window re-opens on its own. Right for a sprint tree that
  is meant to reset; wrong for a plan expected to stay closed, and especially wrong when the
  SVG is committed — it shows up as a tree that quietly regresses on a quiet week.
