# Prompt — turn a scaffolded draft into a plan

**Use when:** you ran `slop-gate progression init` and have a `progression.yaml` draft whose
milestones all describe work that already landed.

**Paste with:** [`spec-reference.md`](spec-reference.md), your draft, and (in a chat window) the
repository layout. In an agentic CLI, point the model at the checkout instead and it will read
them itself.

---

```text
You are helping me turn a scaffolded progression plan into a real one.

BACKGROUND
`slop-gate progression init` mined the draft below from my repository's commit history. Every
milestone in it therefore describes work that has ALREADY LANDED — history can say what a
repository has done, never what it meant to do. If I commit it unchanged, the tree reads ~100%
on day one and never moves again. Your job is to make it a plan: a small, honest root of
finished work, and the milestones we are actually planning hanging off it.

WHAT I AM GIVING YOU
1. The format digest (pasted separately) — follow it exactly; a spec that breaks its rules will
   not load.
2. The mined draft:

<<<DRAFT
{{PASTE progression.yaml HERE}}
DRAFT

3. What we are actually planning, in my words:

<<<PLAN
{{2-10 sentences. The next quarter or release. What "done" looks like. What is blocked on what.
Paste a roadmap, an RFC or a list of issue titles if you have one — rough is fine.}}
PLAN

4. The repository layout:

<<<TREE
{{git ls-files | awk -F/ 'NF>1 {print $1"/"$2}' | sort -u | head -80}}
TREE

RULES — the first three are the ones that ruin a plan

1. NEVER INVENT A PATH. Use globs that appear in the draft, or that you can see in the layout,
   or — if you can run commands — that you have confirmed with `git log --oneline -- <dir>`.
   A glob matching nothing renders exactly like a milestone nobody has started, so the failure
   is silent. One exception, and it is important: for PLANNED work, a path that does not exist
   yet is correct. Naming `src/api/ratelimit.rs` before it exists is how the plan says where the
   work will go. The rule is directional — unmatched is fatal for finished work, expected for
   planned work. List every such path at the end (see OUTPUT).

2. COMPRESS THE FINISHED WORK. The draft's mined milestones are the tree's ROOTS, not its
   content. Collapse them into at most 2-3 baseline milestones worth 10-15 XP each. Detail there
   buys nothing: nobody is surprised that the service we have been running for two years exists.

3. EVERY PART MUST BE FALSIFIABLE. Each one names a file, a directory, or a subject convention
   this team actually writes. "Improve performance" and "better tests" are not parts. If a piece
   of work genuinely leaves no trace in the repository — a drill held, a vendor signed — point
   the rule at the note it will leave (`paths: ["docs/drills/**"], subject: "failover"`) rather
   than at nothing.

4. `manual: true` CLOSES a part by hand. It is not a placeholder for "not decided yet" — a
   milestone written that way renders as finished the moment it is scaffolded. Use at most one
   manual part in the whole file, and only for work that has actually happened.

5. MECHANICS (the validator rejects these outright): ids unique; every `requires` names a node
   that exists; no cycles; every part has a selector; never all-zero thresholds; `commits: 0`
   only alongside `prs:`.

6. KEEP IT SMALL. 5-9 milestones, 2-4 parts each. A tree nobody can hold in their head is a tree
   nobody reads. Most of the XP belongs to OPEN milestones — that is the whole point.

7. Give the plan a spine. Use `requires` so the canvas reads left to right: what we had → what
   we are building → what shipping means. A milestone may require two others; that is why the
   node set is a DAG rather than a literal tree.

OUTPUT
First, the complete YAML file and nothing else — no prose before it, no fences around it beyond
one ```yaml block. Comment the non-obvious choices inside the file, briefly.

Then, after the YAML, exactly two short sections:

  UNVERIFIED PATHS — every glob you used that you could not confirm exists, one per line, each
  marked (planned) or (uncertain). I will check the uncertain ones myself.

  WHAT I ASSUMED — any place you had to guess at what we meant, one line each. Ask questions
  here rather than inventing an answer.
```

---

## After it answers

```bash
# 1. It loads and the mechanics are sound (this also catches cycles and unwired parts):
slop-gate progression --spec progression.yaml --format text

# 2. Every path it marked "uncertain", and every milestone you believe is already done:
git log --oneline -- src/api/ratelimit | head

# 3. Sanity-check the shape. Most of the XP should be unearned, and the percentage low.
```

A good first result looks roughly like this — a small done root, most of the tree open:

```
Platform — 2026 Q2
  level 2 · 12% · 40/330 XP · 2/8 milestones · 4/19 parts

  [   Done    ] Gateway                        1/1 parts  10 XP
  [   Done    ] Event pipeline                 1/1 parts  10 XP
  [In progress] Two regions, one control plane 1/3 parts  40 XP
  [  Locked   ] Self-serve onboarding          0/2 parts   0 XP
```

If instead it comes back at 90% done, the model retro-fitted the plan: tell it "rule 2 — these
milestones describe work that already landed; replace them with what we are planning" and give
it another pass.
