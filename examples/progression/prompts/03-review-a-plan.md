# Prompt — audit a plan before you commit it

**Use when:** you have a `progression.yaml` — hand-written, or from one of the other prompts —
and want it checked before it becomes the thing your README shows for the next quarter.

**Paste with:** [`spec-reference.md`](spec-reference.md), the spec, and the output of
`slop-gate progression --spec progression.yaml --format text`. That last one matters: it turns
"does this look right" into "does this say what the repository says".

---

```text
Audit this progression spec. Be blunt; I would rather fix it now than explain it in a month.

<<<SPEC
{{PASTE progression.yaml HERE}}
SPEC

<<<RESOLVE
{{PASTE the output of: slop-gate progression --spec progression.yaml --format text}}
RESOLVE

<<<TREE
{{git ls-files | awk -F/ 'NF>1 {print $1"/"$2}' | sort -u | head -80}}
TREE

CHECK, IN THIS ORDER

1. GLOBS THAT MATCH NOTHING. Cross-read the spec against the resolve output. Any part at 0
   commits whose milestone describes work that sounds FINISHED is a broken glob, not an
   unstarted task — the two are indistinguishable on the canvas, which is why this is first.
   For each, give me the `git log --oneline -- <dir>` command that settles it.

2. RETRO-FIT. What share of the XP is already earned? If the tree is mostly done on the day it
   is written, it will not move again. Say which milestones describe work that already landed
   and should be compressed into a baseline root.

3. UNFALSIFIABLE PARTS. Any part that could not be honestly closed or left open by looking at
   the repository. Rewrite each one as a rule, or tell me it should be cut.

4. SHAPE. Does the `requires` graph read left to right? Any milestone that is really two? Any
   part that is really a milestone? Anything locked behind something unrelated?

5. THRESHOLDS. `commits:` and `prs:` numbers that are either trivially met or unreachable. A
   `prs:` threshold in a repo whose merges carry no PR marker never closes — check the resolve
   output's PR count before trusting any of them.

6. MECHANICS the validator would catch anyway (cycles, duplicate ids, unwired parts, all-zero
   thresholds) — mention only if present.

OUTPUT
A numbered list of findings, worst first. For each: the node and part id, one sentence on what
is wrong, and the concrete fix (a YAML fragment, or the command that would settle it). End with
one line: would you commit this as it stands, yes or no, and if no the single change that would
matter most.
```

---

## Reading the answer

Findings under (1) and (2) are the ones worth acting on before committing — they are the two
failures the tooling cannot catch and that make a tree lie for months. (3) is usually a wording
fix. (4) is taste, and yours overrules the model's.
