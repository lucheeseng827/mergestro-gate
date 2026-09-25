# Progression by example

Everything you need to go from a repository with no plan to a tree in its README, plus the
prompts for having an assistant do the authoring part with you.

```
examples/progression/
├── try-it.sh            a full run on a throwaway repo — read the output, then delete it
├── specs/               three finished plans, for three shapes of repository
└── prompts/             paste-ready prompts for Claude, GPT, or any capable model
```

Start here if you have five minutes:

```bash
./try-it.sh          # builds nothing, needs `slop-gate` on PATH or $SLOP_GATE
```

## The three steps, and which one is actually hard

```bash
slop-gate progression init --repo .            # 1. mine a draft
$EDITOR progression.yaml                       # 2. make it a plan          ← the hard one
slop-gate progression --spec progression.yaml \
  --svg docs/progression.svg --readme README.md # 3. draw it
```

Step 1 is mechanical: it reads the directories that carry work and the marker your merges write,
because those are the two things a hand-written first spec gets wrong *silently* — a glob that
matches nothing renders exactly like a milestone nobody started.

Step 3 is mechanical too. Put the marker pair in your README first, where the tree belongs:

```markdown
<!-- mergestro:progression:start -->
<!-- mergestro:progression:end -->
```

Step 2 is the one nothing can do for you, and the reason is worth understanding before you
start editing: **history says what a repository has done, never what it meant to do.** So the
draft describes finished work, and if you commit it unchanged you get a tree that is 100% done
on the day you adopt it. The draft leaves one milestone open (`next`) as the reminder. Your job
in step 2 is to make that milestone — and usually two or three more — real.

## How to read the resolve

Run step 3 without writing anything and read the summary:

```bash
slop-gate progression --spec progression.yaml --format text
```

```
[   Done    ] Ingest pipeline           3/3 parts  60 XP
[In progress] Public API                1/3 parts  20 XP
    [x] Routes and their contract       7/1 commits
    [ ] Rate limiting                   0/1 commits   ← is this planned, or is the glob wrong?
[  Locked   ] GA                        0/0 parts   0 XP
```

One rule interprets every `0/1` you see:

* The milestone is **planned** → `0/1` is correct. That is the plan, and it will tick over when
  the work lands. Paths that do not exist yet are fine here.
* The milestone is **already done** and still reads `0/1` → **your glob is wrong.** Check it:

  ```bash
  git log --oneline -- src/api/ratelimit | head   # drop the /** and ask git
  ```

  No output means no commit ever touched that path, and no future resolve will close the part.

That one check catches the single most common authoring mistake, and it costs a second.

## Common mistakes, in the order people make them

| Mistake | What you see | Fix |
| --- | --- | --- |
| Committing the draft unedited | 11/12 milestones done on day one | Replace the frontier; delete milestones nobody planned |
| A glob for a directory that was never called that | A "done" milestone stuck at 0/1 | `git log --oneline -- <dir>` |
| Milestones for work already finished | The tree stops moving | Compress finished work into one or two baseline milestones; spend your XP on what's open |
| `manual: true` as a placeholder | The milestone is instantly **done** | `manual` *closes* a part by hand. For "not yet decided", use a `subject:` rule nothing matches yet |
| A sprint tree with `since_days` | Closed milestones re-open by themselves | Correct behaviour for a sprint, wrong for a plan meant to stay closed — see `specs/sprint.yaml` |

## The examples

| File | For | Shows |
| --- | --- | --- |
| [`specs/fresh-service.yaml`](specs/fresh-service.yaml) | A repo a few weeks old | A plan that is mostly *open* and fills in as work lands |
| [`specs/monorepo-quarter.yaml`](specs/monorepo-quarter.yaml) | An established repo adopting a plan | Finished work compressed into baseline roots, phases, a real frontier |
| [`specs/sprint.yaml`](specs/sprint.yaml) | Two weeks of work | `since_days`, and what the sliding window costs |

Every one of them is parsed and validated by this crate's test suite, so they cannot rot into
examples that do not load.

## Doing step 2 with an assistant

[`prompts/`](prompts/) has the prompts, and its README says what to paste and in what order.
They are written for any capable chat model — Claude, GPT, or whatever you run — and there is a
shorter path for agentic CLIs, which can verify their own globs against `git log` as they write.

The prompts are not "write me a plan" wrappers. Most of their text is defence against the three
things a model reliably gets wrong here: inventing paths that do not exist, describing finished
work instead of planned work, and writing milestones nothing can ever falsify.

## Then keep it fresh

The refresh belongs in CI, not in your habits: a tree moves when a PR merges, not when
somebody remembers to re-run the command. [`../../docs/PROGRESSION.md`](../../docs/PROGRESSION.md)
§3 has the two postures worth running — validate on a pull request, regenerate and commit on
`main` — and why `--check` belongs on the second one and not the first.
