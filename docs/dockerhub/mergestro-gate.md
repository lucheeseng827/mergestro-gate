# Mergestro Gate (`mancube/mergestro-gate`)

**A behavioral merge gate for AI-generated code — it mutation-tests your diff and blocks merges whose new code isn't actually covered by tests.**

LLM-written code compiles, looks plausible, and ships with tests that assert
nothing. Mergestro Gate runs **differential mutation testing** on the changed
lines: it mutates your diff, reruns the suite, and any mutant that survives is a
line your tests don't really check. It also runs static **slop**, **security**,
and **convention** lanes over the same diff. One verdict, gate-able in CI.

- **Image:** `mancube/mergestro-gate` — `slop-gate` CLI on **Alpine**, with the Rust toolchain (from `rust:1-alpine`) + **`cargo-mutants`** baked in, so the mutation gate runs **fully in-container** (no toolchain to wire up). ~280 MB compressed.
- **Arch:** `linux/amd64` + `linux/arm64` (multi-arch manifest) · **Runs as:** nonroot (uid 65532)
- **Binary inside:** `/usr/local/bin/slop-gate` (entrypoint) · **Workdir / mount point:** `/work`
- **Source / full docs:** github.com/lucheeseng827/mergestro-gate · Apache-2.0

## Where it fits

Mergestro Gate is the **merge gate** of a code-review pipeline: it sits between a
PR (a diff plus its CI checks) and the protected branch, and decides whether the
change earns the merge. It owns the "is this diff actually tested?" verdict and
nothing else — no queue, no webhook, no long-running service.

```
   UPSTREAM                 MERGESTRO GATE             DOWNSTREAM
   (a PR + its checks)      (this image)               (branch protection)

 ┌──────────────┐
 │ AI agent /   │ opens PR ─┐
 │ developer    │           │
 └──────────────┘           │   ┌───────────────────┐
 ┌──────────────┐           │   │  slop-gate        │  PASS  ┌─────────────┐
 │ Git host     │ diff +    ├─▶ │ mutate the diff · │──────▶ │ protected   │
 │ GitHub/GitLab│ base ref ─┤   │ re-run the suite ·│  merge │ branch      │
 └──────────────┘           │   │ static lanes →    │        └─────────────┘
 ┌──────────────┐           │   │ one verdict       │
 │ CI · the     │ suite ────┘   └────────┬──────────┘
 │ suite runs   │                        │ BLOCK · exit 2
 └──────────────┘                        └─▶ PR comment → author / agent
```

- **Upstream** — a PR against a base ref, plus a green CI suite. The gate reads a
  git checkout, not a webhook: CI (or the GitHub Action) invokes it inside a job,
  passing the base ref to diff against. It does not open or poll PRs itself.
- **mergestro-gate** — diffs `base→head`, mutation-tests the changed lines,
  runs the static lanes (slop / security / convention / debt), and reduces it all
  to **one verdict**: exit `0` pass, exit `2` block.
- **Downstream** — a PASS clears the change to merge; a BLOCK (exit `2` on a
  *required* check) holds the protected branch and posts a survivor PR comment
  back to the author or generating agent.

## Tags

| Tag | Notes |
|---|---|
| `latest` | newest release |
| `0.6.1` | pinned version (= current `latest`) |
| `0.6`   | latest `0.6.x` |
| `0.5`   | latest `0.5.x` |

Pin a version in CI: `mancube/mergestro-gate:0.6.1`.

## New in 0.6.0

- **Faster by default** — one suite run before the first mutant (was up to
  three), and each mutant runs only the changed crate's tests
  (`--test-workspace` restores whole-workspace tests).
- **`--budget 10m`** — a wall-clock limit on the mutation run; unfinished
  mutants are reported as *not tested*, never as caught or surviving
  (`--block-on-budget` to make that block).
- **`--sarif out.sarif`** — findings as SARIF 2.1.0 for code scanning.
- **Weakened-test lane** — flags a change that deletes tests or removes
  assertions, which mutation testing cannot see
  (`--block-on-pattern weakened-tests` to block).
- **Since the last run** — the PR comment reports new · still open · resolved;
  `--comment-inline` also posts survivors on their line (GitHub).
- **`--shard k/n` + `slop-gate merge-reports`** — split a large run across CI
  jobs and publish one verdict.

Details and examples: the
[0.6.0 feature guide](https://github.com/lucheeseng827/mergestro-gate#using-the-060-features).

## What's inside

- **`slop-gate`** — the gate CLI (entrypoint).
- **`cargo` + `rustc`** + **`cargo-mutants`** — the Rust mutation engine compiles
  and tests each mutant, so it needs a toolchain at run time. Baked in.
- **`git`** — merge-base / diff against the base ref.

The non-Rust engines (Python / JS / Go / JVM) are advisory PoCs and are **not**
bundled — add their toolchains yourself if you need them.

## Quick start

The container's workdir is `/work` — **mount your repo there**. It must be a git
checkout with the base ref fetched (e.g. `git fetch origin main`).

```bash
# Behavioral gate over the working tree's diff vs origin/main:
docker run --rm -v "$PWD:/work" mancube/mergestro-gate:0.6.1 \
  --repo /work --base origin/main

# Predict the mutant workload first, without building/testing (fast):
docker run --rm -v "$PWD:/work" mancube/mergestro-gate:0.6.1 \
  estimate --repo /work --base origin/main

# Summarise telemetry from prior runs (--metrics-file output):
docker run --rm -v "$PWD:/work" mancube/mergestro-gate:0.6.1 \
  analyze --metrics-file /work/slop-gate-metrics.jsonl

# Help / version:
docker run --rm mancube/mergestro-gate:0.6.1 --help
```

> The gate compiles + tests every mutant in your diff, so wall-clock scales with
> diff size × suite time. Use `estimate` to size a run before committing to it.

## As a CLI base layer

Prefer your own toolchained image? Copy the binary in and bring your own
`cargo-mutants`:

```dockerfile
FROM rust:1-bookworm
COPY --from=mancube/mergestro-gate:0.6.1 /usr/local/bin/slop-gate /usr/local/bin/slop-gate
RUN cargo install cargo-mutants
ENTRYPOINT ["slop-gate"]
```

## Multi-arch

Every release is published for **`linux/amd64` and `linux/arm64`** under one
multi-arch manifest, so `docker pull` picks the right one for the host (x86-64
runners, Apple Silicon, Graviton). Each architecture is built natively, not
under emulation.

## In CI

For GitHub Actions, the
[Mergestro Gate Action](https://github.com/lucheeseng827/mergestro-gate) is
simpler than wiring this image into a job (it runs on a toolchained runner). Use
this image for local runs, non-GitHub CI, and as a self-contained gate
environment.

## Links

- **Source & docs:** https://github.com/lucheeseng827/mergestro-gate
- **License:** Apache-2.0
