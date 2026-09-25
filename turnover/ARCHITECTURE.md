# turnover — architecture

## Data path

```
git objects ──gix──▶ CommitMeta (sha, first parent, time, author)      [history::list_commits, sequential]
                │
                ▼  rayon, one thread-local Repository per worker
             tree diff vs first parent (renames + copies detected)   [history::load_commit]
                │  for each countable blob: old text, new text
                ▼
             significance masks per side                              [lang::mask → core::line]
                │  tree-sitter comment/docstring ranges, or the heuristic
                ▼
             classify: added / deleted / moved / copy_pasted / dup_block [core::signals::classify]
                │  one CommitSignals row + fingerprints of eligible adds/deletes
                ▼
             churn attribution, one ordered pass over fingerprints    [core::churn::attribute]
                │  pending additions carried into the baseline for the next refresh
                ▼
             Baseline (JSON, one row per commit, append-only)         [core::baseline]
                │
                ▼
             window vs baseline aggregates → ratios                   [core::window]
                │
                ▼
             policy: absolute caps + drift limits → Verdict, exit code [core::policy]
                │
                ├─▶ text report / --json report                        [cli]
                └─▶ Mergestro record (JSON-Lines, record_type: turnover)[cli::record]
```

## Crate DAG and why it splits this way

```
cli ──▶ gate ──▶ history ──▶ lang ──▶ core
         │                            ▲
         └────────────────────────────┘
```

* **core** is PURE: strings in, counts out. No git, no filesystem, no clock, no C
  build. A classification bug is the only way the gate can fail a build for the wrong
  reason, so this surface must stay auditable line-by-line and testable with nothing but
  strings. `cargo test -p turnover-core` is the complete test of the correctness-critical
  model and builds in seconds.
* **lang** owns the only C build (tree-sitter + six grammars). It exports one thing to
  core's world: a per-line significance mask. The AST never crosses the boundary — the
  three v0 signals are line-identity signals, and tying them to syntax would make them
  incomparable the moment one grammar is missing. Error-masking detection is where this
  crate grows, and it will still hand core a mask, not a tree.
* **history** owns git. It decides what is *countable* (not vendored, not binary, not
  oversized, has a language) and hands core both sides of every file. It also owns the
  parallelism: commits are independent once listed, so this is `par_iter().map_init(..)`
  with a `ThreadSafeRepository::to_thread_local()` per worker and a 64 MiB object cache
  each.
* **gate** is the gate as a library: the TOML policy, the baseline file on disk (build,
  refresh), window selection (trailing or PR scope), evaluation, the text/Markdown reports
  and the Mergestro record. The `turnover` binary and the turnover lane inside the
  Mergestro gate (`mergestro-gate`) both call it and nothing else, so a change fails for the
  same reason on both paths.
* **cli** is argument parsing over `gate`. Nothing in it makes a decision.

## The baseline file

`.turnover/baseline.json`, format version 1:

| field | meaning |
|---|---|
| `head` | the tip the newest walk started from; the next refresh hides everything below it |
| `config` | the `[signals]` settings the rows were produced with; a mismatch refuses to mix rows |
| `commits[]` | one `CommitSignals` per commit: sha, parent, time, author, `counts`, `by_language` |
| `churn_horizon_secs` | the horizon churn was attributed with |
| `pending[]` | additions still inside the churn horizon at the newest row, grouped per commit and path |

`pending` is what makes incremental refresh correct: without it the last horizon before
every refresh would be blind to churn. It is stored grouped (one sha and one path string
per group, then a list of fingerprints) because a two-week tail of a busy repository is
tens of thousands of lines: one JSON object per line was a 7 MB baseline for this
repository's 50 commits in the first measurement; grouped, the same baseline is 0.9 MB.

## Choosing the window

* **Trailing window** (default): `to = newest commit + 1s`, `from = to − window_days`. The
  baseline is every row older than `from`.
* **PR scope** (`--base-ref X`): the window is the set of commits reachable from HEAD but
  not from X; the baseline is every other row. This is "the delta this PR moves" and is
  what the Action uses on `pull_request` events.

Both go through the same `evaluate(policy, baseline, window)`, and the same function runs
on ratios alone (`evaluate_ratios`) so a control plane can re-judge an ingested window
without the rows.

## Performance notes

* The walk is CPU-bound on tree-sitter parsing of both sides of every changed file.
  `max_file_bytes` (1 MiB) and the generated-file exclusions are the levers; the
  parse happens once per side, and the mask is what survives.
* Rename and copy detection use gix's rewrite tracking at 50% similarity, restricted to
  the commit's own modified set (`CopySource::FromSetOfModifiedFiles`), so it stays
  O(changes²) per commit rather than O(changes × tree).
* Churn attribution is O(fingerprints) with a per-key `VecDeque`; it is the one sequential
  stage and has not been the bottleneck at any size measured so far.
* Measured on this repository (50 commits, 46k significant added lines, 1030 files): 4.7 s
  wall on the session container, ~15 s CPU.
