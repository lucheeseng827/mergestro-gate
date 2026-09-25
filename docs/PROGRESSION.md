# Progression — the plan, drawn, and closed by the repository

The gate answers *is this change safe to merge?*. Progression answers the question next to it,
which no single CI run can: **where are we against the plan we wrote?**

A plan document tells you what was intended. A commit log tells you what happened. Neither tells
you how far apart they are. Progression puts both in one artifact: an authored milestone tree
whose every part carries an **evidence rule** the repository itself satisfies — commits under
these paths, this many distinct PRs, a subject matching this pattern — and resolves it into a
picture you can put in a README and on a console.

A repository that does not have a plan yet starts with
[`slop-gate progression init`](#0-start-from-a-draft), which mines a draft from its own history.
Then: three consumers, one snapshot.

| Output | Flag | Who reads it |
| --- | --- | --- |
| The committed SVG | `--svg` | Anyone opening the repository |
| The README block | `--readme` | The same, without leaving the page |
| The JSON snapshot | `--json` | Tooling. The `ProgressionSnapshot` *payload* — `--plane-url` wraps it with the record kind and CI identity to make an ingest record, so this file is not a body you can POST |
| The console canvas | `--plane-url` | An operator watching a fleet of plans |
| A job-log summary | `--format text` | Whoever is reading CI |

## 0. Start from a draft

```bash
slop-gate progression init --repo . --out progression.yaml
```

A first spec is mostly typing, and the two things it most often gets wrong are silent. A path
glob that matches nothing renders exactly like a milestone nobody has started, and a
`pr_pattern` that does not match this repository's merges makes every `prs:` threshold sit at
zero forever. Both are knowable from the log, so `init` reads them out of it: the directories
that actually carry work, and the marker the merges actually write.

It stops there, and the stopping point is the design. A milestone is a claim about **intent**,
and intent is not in the log — so everything mined describes work that has *already landed*.
The draft says so in its own comments, and ends with one milestone left open:

```yaml
  - id: next
    title: What's next
    requires: [phase_3]
    parts:
      - id: define
        title: Name the work, then wire it to evidence
        evidence: { subject: '\[progression:next\]' }
```

That rule matches a tag no commit carries, so the milestone stays open until somebody replaces
it — which is also this tool's answer to the **retro-fit problem**. A plan written today for a
five-year-old repository resolves to ~100% on the first run. You can hide that behind a sliding
`since_days`, at the cost of milestones that re-open when the window moves past them; or you can
say the true thing, which is that the finished part is the tree's *roots* and the plan proper
starts at the frontier. The scaffold says the true thing, and prints what the draft reads as so
nobody discovers it a week later:

```
  mined 8 component(s) from 2760 commit(s); 12 milestone(s) in the draft
  PR marker: GitHub squash merges — `… (#123)` on 19% of commits
  · <component path> — <commits> commits, <files> files     (one line per component)
  reads now: 11/12 milestones done · level 3 · 95%
```

### What it mines

| Decision | Rule |
| --- | --- |
| Which directories | Counted **once per commit**, so one 400-file sweep cannot outrank a year of work. Build output, vendored trees and lockfiles are skipped. |
| How deep | A directory whose work spreads over two or more named children is a *container* and gets split (`services` → `services/auth`, …); one whose only child holds nearly all of it is a *wrapper* and gets descended through (`code` → `code/services`). `src`, `docs`, `tests` and their kin are never components — they are how one component is laid out, so they become its parts. |
| Which order | Oldest first, by when each component's first commit landed, grouped into phases with a marker milestone between them. Only when the walk saw the whole history: inside a `--since-days` window, or past a `--max-commits` cap, everything "first appeared" at the edge, so the draft is flat instead. |
| The PR marker | The most specific candidate that matches ≥10% of subjects. `#(\d+)` matches everything the others do — and every issue reference besides — so it is tried last, not first. |

### A repository with no history yet

Under two components, there is nothing to mine, and `init` writes a **starter plan** instead:
Foundations → The core path → Hardening → Ship it, wired to conventional locations
(`src/**`, `.github/workflows/**`, `tests/**`, `docs/**`). It is the better shape anyway. A
scaffold mined from a mature repository can only ever start at 100%; a starter plan on a new one
resolves near zero and **fills in as the work lands**, which is what a progression tree is for:

```
[In progress] Foundations          2/3 parts  20 XP
    [x] The source tree exists     2/1 commits
    [x] Dependencies are declared  1/1 commits
    [ ] CI runs on every push      0/1 commits
[In progress] The core path        0/2 parts   0 XP
    [ ] The main path is built out 2/10 commits
```

### Flags

| Flag | Effect |
| --- | --- |
| `--repo <path>` | Repository to mine. Default `.`. |
| `--out <path>` | Where to write. Default `progression.yaml`. Refuses to overwrite without `--force`. |
| `--head <ref>` | Ref whose ancestry to mine. Default `HEAD`. |
| `--title` / `--season` | Plan heading and period label. Title defaults to the repository's directory name. |
| `--since-days <n>` | Mine only the last N days, and write that window into the draft (with a comment about what a sliding window does to a closed milestone). |
| `--max-nodes <n>` | Cap on mined milestones. Default 8. |
| `--min-commits <n>` | Commits a directory needs to qualify. Default 3. |
| `--depth <n>` | How many segments deep a component may sit. Default 3. Raise it for a deeply nested monorepo. |
| `--max-commits <n>` | Stop the walk after this many commits (default 20000). |
| `--stdout` | Print the draft instead of writing it. |
| `--force` | Overwrite an existing file. |

A shallow clone is **not** refused here the way it is for a resolve — a draft is a draft — but it
is reported, and it drops the phases, because a checkout that can only see last week cannot say
which component came first.

Then edit it. Rename the milestones to the work rather than the directory, delete what was never
part of the plan, and replace the frontier with what you are actually planning. The draft is
round-tripped and validated before it is written, so whatever you start from parses.

[`examples/progression/`](../examples/progression/) is that step worked through: `try-it.sh`
runs the whole loop on a throwaway repository, `specs/` holds three finished plans (a young
service, an established repo adopting one, a two-week sprint), and `prompts/` has paste-ready
prompts for authoring the plan with an assistant — most of whose text is defence against the
three things a model reliably gets wrong here: inventing paths that do not exist, describing
finished work instead of planned work, and writing milestones nothing can falsify.

## 1. Author the plan

Start from [`mergestro-progression.example.yaml`](../mergestro-progression.example.yaml), which
annotates every key. The shape:

```yaml
version: 1
title: Acme API — 2026 H1
season: 2026 H1
nodes:
  - id: ingest
    title: Ingest pipeline
    paths: ["src/ingest/**"]          # scopes every part below
    parts:
      - id: validate
        title: Reject a malformed batch at the door
        xp: 20
        evidence: { paths: ["src/ingest/validate.rs"] }
  - id: api
    title: Public API
    requires: [ingest]                # locked until `ingest` is done
    parts:
      - id: routes
        title: Routes and their contract
        evidence: { paths: ["src/api/**"], prs: 2, commits: 0 }
```

`requires` makes the node set a **DAG**, not a literal tree: a milestone routinely needs two
predecessors, and forcing one parent would make the author pick a lie. "Tree" is what it reads as
on the canvas.

### Evidence rules

Every declared condition is ANDed. `paths` and `subject` *select* commits; `commits` and `prs`
are thresholds over that selection.

| Key | Meaning |
| --- | --- |
| `paths` | Repo-relative globs. `**` crosses directories, `*` does not, `?` is one non-`/` character. A pattern with no wildcard also covers everything beneath it, so `docs` matches `docs/CONFIG.md`. |
| `subject` | Regex over the commit subject **and body**, case-insensitive — so a trailer matches too. |
| `authors` | Substring match on the commit author. Empty means anyone. |
| `commits` | How many matching commits close the part. Defaults to 1 when a selector is set. |
| `prs` | How many *distinct* PRs among the matching commits. Set `commits: 0` for a PR-only rule. |
| `manual` | Closed by hand. The escape hatch for work no commit can prove — a review held, a vendor signed. |

Two rules exist to stop a progress number that can be gamed by committing:

* **A part with no selector closes nothing.** `commits: 3` on its own would otherwise be closed by
  any three commits in the repository.
* **A milestone cannot be `done` while something it requires is open.** Work that landed out of
  order shows as `in progress`, not as a plan completed in sequence.

A node with **no parts** is a marker: it closes as soon as its requirements do. That is how a
spec says "and then we shipped" without inventing a fake task to tick.

### States, XP and levels

| State | Meaning | On the canvas |
| --- | --- | --- |
| `locked` | A requirement is not done | Grey, dimmed title |
| `available` | Unblocked, nothing started | Dashed outline |
| `in_progress` | Some evidence landed, not every part closed | Yellow |
| `done` | Every part closed **and** every requirement done | Teal |

A part is worth `xp` (default 10); a node's total is the sum of its parts, and earned XP is the
sum of the closed ones. `levels` is a list of cumulative thresholds — omit it for the built-in
eight-band default, whose first band is reachable in a day so a new tree is not stuck at level 1
for a quarter.

## 2. Resolve and render

```bash
slop-gate progression \
  --spec progression.yaml --repo . \
  --svg docs/progression.svg \
  --readme README.md
```

Put the marker pair in the README where the tree belongs; the command owns everything between
them and nothing outside:

```markdown
<!-- mergestro:progression:start -->
<!-- mergestro:progression:end -->
```

| Flag | Effect |
| --- | --- |
| `--spec <path>` | The plan (required). |
| `--repo <path>` | Repository to resolve against. Default `.`. |
| `--head <ref>` | Ref whose ancestry is the history. Default `HEAD`. |
| `--since-days <n>` | Ignore older commits (overrides the spec). |
| `--svg` / `--markdown` / `--readme` | Write that artifact. Deterministic — safe to commit. |
| `--json` | Write the `ProgressionSnapshot`. **Not** a committed artifact (it carries the resolve time, so a committed copy differs on every run) and **not** an ingest body — `--plane-url` wraps the same snapshot with the record kind and identity. For piping and inspection. |
| `--svg-href <src>` | `src` for the README's `<img>`. Defaults to `--svg` made relative to the README's directory. |
| `--check` | Write nothing; **exit 2** if any requested artifact would change. |
| `--max-commits <n>` | Stop the walk after this many commits (default 20000). |
| `--allow-shallow` | Resolve against a truncated history anyway (see below). |
| `--plane-url <url>` | POST the snapshot to a Mergestro plane. Token: `METRICS_TOKEN`. Skipped under `--check`. |
| `--slug <owner/repo>` | Repo the snapshot is filed under. Defaults to `GITHUB_REPOSITORY`. |
| `--format text\|json\|markdown` | What the command prints. Default `text`. |

**Shallow clones are refused.** `actions/checkout` defaults to depth 1, and a plan resolved
against the last commit reads as barely started — wrong, and indistinguishable from real
regression. Check out with `fetch-depth: 0`. `--allow-shallow` exists for the cases where you
genuinely mean it, and warns.

**A capped walk is reported.** The walk stops at `--max-commits` (default 20000) so a decade-old
monorepo cannot turn a docs job into a timeout. If it hits the cap with ancestry still unread it
says so — silently understating every milestone older than the cap is the same failure a shallow
clone causes, and it gets the same treatment. Raise the cap when you see it.

**Merge commits are skipped.** A merge's first-parent diff re-attributes every file on the merged
branch to one commit, which would close a milestone on the strength of a merge that "touched" a
thousand paths. The branch's own commits are in the walk anyway.

**PR numbers are parsed from the subject,** not fetched: no token, no rate limit, no network on a
docs job. Override `pr_pattern` for a forge that marks them differently, or count commits instead.

## 3. Keep it fresh

The repository's `mergestro-progression` workflow runs two postures against the same command:

* **On a PR** touching the spec or the renderer: resolve and render to the runner's temp dir. A
  cycle, a dangling `requires`, a bad glob or a panicking renderer fails the PR rather than the
  refresh job on `main`.
* **On a merge to `main`, and weekly:** regenerate and commit if anything moved. Weekly matters —
  a tree moves when a PR merges, not only when the spec changes.

The SVG is committed rather than rendered on demand so the README works on a fork, in a mirror,
and in a tarball with nothing running behind it. It carries **no wall clock** so that a refresh
with nothing to say produces no diff.

### Where `--check` belongs

A resolve is a function of the whole history at `HEAD`, so committing the drawing *changes the
history it describes*. On a PR the branch's own commits are already in the walk, and the committed
SVG is legitimately one resolve behind — asserting it there is a check that cannot pass. Use
`--check` on `main`, where the refresh job has just written the artifact it compares against, or
as a local "is my copy current?".

## 4. On the console

With `--plane-url`, each resolve is uploaded as a `progression` ingest record and the Mergestro
console draws it on the **Progression** screen: the fleet board (least-advanced plan first, and
what closed since the previous resolve) beside an interactive canvas of one repo's tree, where
clicking a milestone shows its parts and the commits and PRs that closed them.

The canvas uses the **emitter's** layout — `col`/`row` travel on the record rather than being
recomputed — so the console and the README show the same tree in the same arrangement. Two
drawings of one graph, differing only in placement, is how a difference in layout gets read as a
difference in state.

Latest-wins per repo: the plan itself moves, and a node the spec deleted last week must not
linger on the canvas because an older record still mentions it. The older records stay, and give
the board its "first seen" and its newly-closed diff — the one genuinely longitudinal thing here,
and unknowable from a checkout once the plan has moved on.

### Finding it from inside the console

The board carries two lists. `rows` is the repos that have a plan. `planless` is the repos
reporting other telemetry that have never published one — newest contact first, with how long ago
they last reported and how many records they have sent.

That second list is the point: without it the board shows only repos that already opted in, which
means the one population that needs to hear about progression is the one population that cannot
see it. Picking a planless repo on the **Progression** screen shows the exact command for *that*
repo, with the plane's own ingest URL already filled in (derived from the console's origin, so it
cannot drift) and the token left as a placeholder — a console that renders a live credential into
copyable text is one screenshot away from leaking it.

Endpoints: `GET /v1/fleet/progression?repo=&limit=` (the board, both lists — `limit` caps each
independently, because on a tenant where nothing has a plan the planless list *is* the answer) and
`GET /v1/progression/{owner}/{repo}` (one tree, `null` when that repo has never published one).
See [`API.md`](./API.md) for the record schema.
