# Prompt — turn a roadmap into a tree

**Use when:** the plan already exists as prose — a roadmap, an RFC, a quarter doc, a list of
issue titles — and you want it expressed as a progression spec. No draft needed, though running
`slop-gate progression init --stdout` first and pasting its output gives the model real path
globs to work with and is worth the thirty seconds.

**Paste with:** [`spec-reference.md`](spec-reference.md) and the repository layout.

---

```text
You are turning a roadmap into a progression spec — a plan the repository itself can close.

WHAT I AM GIVING YOU
1. The format digest (pasted separately). Follow it exactly; a spec that breaks its rules will
   not load.
2. The roadmap:

<<<ROADMAP
{{Paste it. Prose, bullets, issue titles, a quarter doc — whatever form it is in.}}
ROADMAP

3. The repository layout:

<<<TREE
{{git ls-files | awk -F/ 'NF>1 {print $1"/"$2}' | sort -u | head -80}}
TREE

THE TRANSLATION
A roadmap item becomes a MILESTONE when it is a thing someone would announce. It becomes a PART
when it is a step on the way there. If an item is neither — a theme, a value, an aspiration —
it does not belong in the tree at all: this format only holds work a commit can close.

For each part, decide what would make it visibly true in this repository, and write that as the
evidence rule:

  - code in a place        → paths: ["src/payments/**"]
  - a specific file        → paths: ["docs/runbook.md"]
  - a convention we write  → subject: "perf:" with commits: 5
  - reviewed change        → paths: [...], commits: 0, prs: 2
  - no trace at all        → point at the note it will leave behind, not at nothing

RULES
1. NEVER INVENT A PATH for work that already exists. For work we are PLANNING, naming the file
   it will live in is correct and expected — that is how the plan says where the work goes. List
   both kinds at the end so I can tell them apart (see OUTPUT).
2. Order with `requires` so the canvas reads left to right. A milestone may require two others.
3. If the roadmap is mostly finished work, say so rather than writing a tree that reads 100% on
   day one. Compress it into one or two baseline milestones and tell me in WHAT I ASSUMED.
4. `manual: true` CLOSES a part. Never use it as a placeholder.
5. MECHANICS: ids unique; `requires` names existing nodes; no cycles; every part has a selector;
   never all-zero thresholds; `commits: 0` only alongside `prs:`.
6. 5-9 milestones, 2-4 parts each. XP roughly proportional to effort, 10 as the unit.
7. Anything in the roadmap you could not place, list it — do not quietly drop it and do not
   invent a milestone to hold it.

OUTPUT
The complete YAML file first, in one ```yaml block, nothing before it. Then exactly three short
sections:

  UNVERIFIED PATHS — every glob you could not confirm, marked (planned) or (uncertain).
  UNPLACED ITEMS — roadmap items that did not become a milestone or a part, and why.
  WHAT I ASSUMED — where you had to guess. Ask, do not invent.
```

---

## After it answers

```bash
slop-gate progression --spec progression.yaml --format text
```

Read the UNPLACED list first — it is usually the most interesting output, because it is where
the roadmap says something the repository cannot check. Sometimes that means the item needs a
sharper definition of done; sometimes it means it genuinely is not engineering work and belongs
somewhere other than this tree.
