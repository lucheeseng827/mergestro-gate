# Prompts for authoring a plan with an assistant

`slop-gate progression init` writes a draft. Turning it into a plan means deciding what the
next quarter is *for*, and a model can genuinely help with that — it can read the repository
faster than you can, and it will suggest milestones you had not written down.

It will also, reliably, get three things wrong. Most of the text in these prompts is defence
against them:

| Failure | What it looks like | Why it matters |
| --- | --- | --- |
| **Invented paths** | `evidence: { paths: ["src/core/scheduler/**"] }` for a directory that does not exist | The part can never close, and on the canvas it is indistinguishable from work nobody has started |
| **Retro-fitting** | Milestones describing what the repo already does | The tree reads 100% on day one and never moves again |
| **Unfalsifiable milestones** | "Improve performance", `manual: true` everywhere | A plan nothing can close is a wiki page with extra steps |

| Prompt | Use it when |
| --- | --- |
| [`01-author-from-a-draft.md`](01-author-from-a-draft.md) | You ran `progression init` and have a draft to turn into a plan. **Start here.** |
| [`02-author-from-a-roadmap.md`](02-author-from-a-roadmap.md) | You already have a roadmap, RFC or issue list and want it expressed as a tree |
| [`03-review-a-plan.md`](03-review-a-plan.md) | You have a spec and want it audited before you commit it |

[`spec-reference.md`](spec-reference.md) is the format digest the model needs. Paste it with
whichever prompt you use — it is written to be pasted, not read.

## What to paste, in what order

**In an agentic CLI** (Claude Code, or any assistant that can run commands in your checkout) —
the best case, because it can check its own work:

```bash
slop-gate progression init --repo .
claude "$(cat examples/progression/prompts/01-author-from-a-draft.md)"
```

Tell it the plan context when it asks. It can read `progression.yaml` and the tree itself, and
the prompt makes it verify every glob with `git log` before writing it down.

**In a chat window** (claude.ai, ChatGPT, anything else) — paste four things:

1. `spec-reference.md`
2. the prompt file
3. your `progression.yaml` draft
4. the repository layout, so it stops guessing at paths:

   ```bash
   git ls-files | awk -F/ 'NF>1 {print $1"/"$2}' | sort -u | head -80
   ```

**Through an API**: `spec-reference.md` as the system prompt, the prompt file plus your draft
as the user message. Ask for YAML only — the prompts already say so, but a `response_format`
or a prefilled assistant turn starting with `version: 1` makes it stick.

## Then check its work — this part is not optional

A model that invents a path fails silently, so the prompts require it to end with a list of
every glob it could not verify. Check that list, then check the whole file:

```bash
slop-gate progression --spec progression.yaml --format text
```

Read every `0/1`. For a **planned** milestone that is correct — the work has not happened yet.
For a milestone you believe is **already done**, a `0/1` means the glob is wrong:

```bash
git log --oneline -- src/core/scheduler | head    # drop the /** and ask git
```

Silence there means no commit ever touched that path, and none of your future resolves will
close that part either.

The tool itself catches the mechanical mistakes — a cycle, a dangling `requires`, a part with
no selector, thresholds that are all zero — and refuses to render until they are fixed. What it
cannot catch is a plausible path that never existed, or a milestone that was already finished
when it was written. Those two are yours.
