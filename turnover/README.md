# turnover — the maintainability lane's crates

**This directory is vendored.** It is the source of the `turnover` crates that the merge
gate path-depends on for its maintainability lane, copied here at publish time so this
repository builds and tests standalone. It is Apache-2.0, it is its own cargo workspace
(`turnover/Cargo.toml`, with its own lockfile), and `cargo test --manifest-path
turnover/Cargo.toml` is run against it on every sync — so what is here is tested on its
own terms, not only as the gate's dependency.

If you are reading this because you found six unexplained crates in someone else's
repository: that is what this file is for.

## What it does

A longitudinal maintainability gate. Every other tool in this space judges one commit at
a time; the damage AI-generated code does is a *trend*, and it has been measured — across
623 million code changes (2023–2026, GitClear/GitKraken), refactoring line moves are down
70% and cross-file function calls down 35%, while copy/paste is up 41%, block duplication
up 81% and two-week churn up 15%.

None of that is visible to a per-commit lint pass. turnover walks a repository's own git
history, classifies each significant added line into the published buckets, and answers
whether *this* codebase drifted against *its own* baseline — with an exit code, not a
dashboard.

* `turnover baseline` — a full-history walk that writes `.turnover/baseline.json`,
  refreshed incrementally afterwards.
* `turnover gate` — fails the build on drift past the baseline, with absolute caps,
  drift limits, an insufficient-sample rule, and `--base-ref` to scope it to a PR.
* `turnover report` / `turnover explain` — the trend series, and a per-line audit of why
  a line landed in the bucket it did.

## The crates

| crate | what is in it |
|---|---|
| `core` | the signal model — pure, no git and no parser runtime. `cargo test -p turnover-core` is the audit of the classification rules. |
| `lang` | tree-sitter significance masks (Rust, Python, JavaScript, TypeScript/TSX, Go, Java), with a heuristic fallback that reports how many lines rested on it. |
| `history` | the `gix` commit walk, tree diffs with rename and copy detection, rayon classification. |
| `gate` | the gate as a library: baseline on disk, window selection, evaluation, reports. This is the crate the merge gate links. |
| `cli` | the `turnover` binary. |

The dependency direction is one-way: `core` knows nothing about git or about parsers, so
a classification rule can be tested without a repository and a grammar can be added
without touching the rules.

## Two things worth knowing before you rely on it

**The baseline is the point, and it is yours.** The gate compares a repository against
its own history, not against an industry threshold, because a cross-repo threshold would
be meaningless for a 15-year-old C codebase and a six-month-old TypeScript service at the
same time. That means the first run is a measurement, not a verdict — and it means the
classified history is a file in your repository rather than a record in somebody's
platform.

**Language coverage is the hard part, not the signal count.** A ratio is only comparable
across a polyglot repository if every language's significance mask is comparable. Six
grammars are covered; everything else falls back to a heuristic, and the report says how
many lines did so. Treat a number carrying a large heuristic fraction as advisory.

## Upstream

This is a vendored copy. The project's own documentation — architecture, roadmap, the
market picture, and the ADRs behind the decisions above — lives with the upstream
project, and `ARCHITECTURE.md` beside this file is the copy that travels with the code.

Licensed Apache-2.0. See `LICENSE` in this directory.
