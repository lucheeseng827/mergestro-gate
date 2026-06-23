# Integration guide — Slop Filter behavioral merge gate

This guide is for two audiences: **CI/CD owners** wiring the gate into a pipeline
and branch protection, and **developers** who hit the gate on a PR (or run it
locally) and need to act on what it reports. It also states plainly **what the
gate does and does not do**, so expectations are right from day one.

- New to the project? Start with the [README](./README.md).
- Want to try it offline first? See the [closed-environment harness](./test-harness/README.md).
- Every knob lives in [`mergestro-gate.example.yaml`](./mergestro-gate.example.yaml).

---

## What it is

A **differential mutation gate** for Rust. On a pull request it takes the diff
against the base branch, makes small behaviour-changing edits ("mutants") to the
changed lines, and re-runs your test suite. A mutant that survives — the tests
still pass with the code broken — is a hole in your tests on exactly the surface
this PR touched. The gate surfaces those survivors, ranks them by risk, and can
block the merge.

The wedge: it gates the code an agent *writes* by **running** it, before it
becomes a PR a human has to read.

## What it does

- **Diff-scoped mutation testing.** Only changed lines are mutated
  (`cargo-mutants --in-diff` for Rust), so cost scales with the PR, not the repo.
- **Python (advisory PoC).** Changed `.py` files are mutation-tested via
  `cosmic-ray`, diff-scoped to the changed lines and fed through the *same*
  verdict, severity ranking and telemetry as Rust (auto-detected by extension).
  See limits below.
- **Determinism pre-flight.** Runs your suite first and requires it to be green
  and stable; flaky/red suites are reported, not mutated (results would be
  meaningless).
- **Behavioral verdict.** Blocks when surviving mutants exceed a budget
  (default: any survivor blocks). Exit code `2` fails a required check.
- **Severity ranking (Phase 4).** Each survivor is tiered `critical`→`low` from
  where it lives (auth/permission/crypto paths rank up; logging/formatting down)
  and what it does (control-flow > arithmetic). Optionally block only at/above a
  tier.
- **Zero-assertion pre-check.** A free static signal: flags changed tests that
  execute code but assert nothing.
- **Debt-delta budget (Phase 4).** Reports the net complexity/duplication/
  coupling a diff adds, framed against a per-PR budget; optionally blocks.
- **Idempotent PR comment.** One comment, updated in place across re-runs.
- **Validation telemetry + trend (Phase 3/4).** Appends a JSON-Lines record per
  run; `slop-gate analyze` reads them back into block rate, fix-vs-override,
  latency, mutation-score trend, and severity mix.

## What it does NOT do

Being explicit here saves disappointment:

- **It is not a substitute for writing tests or for code review.** It tells you
  *where* tests are weak on the changed surface; it doesn't write tests, fix
  bugs, or judge design. Pair it with human/AI review (e.g. CodeRabbit), which
  it complements rather than replaces.
- **It does not prove correctness.** A clean run means "no *cheap* mutant
  survived on the changed lines," not "this code is correct." Mutation testing
  samples a class of faults; it can't catch what it doesn't mutate.
- **It only covers the diff.** Untouched code is never mutated. It is a *merge
  gate*, not a whole-repo audit. (Run `cargo-mutants` directly for a full sweep.)
- **Rust + Python (PoC) + JS/TS + Go + Java/Kotlin (PoCs).** Beyond Rust, each
  language is an advisory proof-of-concept. The Python path uses `cosmic-ray`
  (`pip install cosmic-ray`; on some setuptools versions its `yattag` dependency
  needs a build workaround). CI install is opt-in via the Action's
  **`enable-python: "true"`** (installs Python + cosmic-ray; your project's own
  test deps like pytest must already be present). The PoCs still lack the Rust
  path's per-function cap, AST-based severity/debt, and zero-assertion check, and
  exec the whole changed file then filter to changed lines (costlier per file
  than Rust's `--in-diff`). The Python **per-function cap is blocked** on
  cosmic-ray having no pre-exec mutant selection — deferred until a custom filter
  or engine swap.
- **It needs a real, green, deterministic suite.** No tests, a red suite, or a
  flaky one means mutation is suppressed — the gate can't certify what it can't
  trust.
- **It runs your tests many times.** Each mutant recompiles and re-tests the
  changed crate. Cost is bounded (`--in-diff` + per-function cap) but non-zero;
  budget CI minutes accordingly (see Latency below).
- **Severity and debt scores are heuristics, not guarantees.** They rank and
  prioritise; they are not a risk certification. Both are advisory unless you
  opt into gating on them.
- **Telemetry is best-effort and local by default.** It never changes a verdict;
  "disable rate / kept-enabled" trends are an ingestion-side concern, not
  computed in-process.

---

## CI/CD integration

> For a standalone, copy-paste **GitHub Action guide** (quickstart, every input,
> exit codes, private-repo token, troubleshooting) see [`ACTION.md`](./ACTION.md).
> The summary below covers the essentials.

### 1. Add the Action

`.github/workflows/pull_request.yml` (see [`ci-example/`](./ci-example)):

```yaml
permissions:
  contents: read
  pull-requests: write          # so the gate can comment
jobs:
  behavioral-gate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with: { fetch-depth: 0 }  # full history for the merge-base
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2   # warm target/ across runs — big win
      - uses: lucheeseng827/mergestro-gate@main
        with:
          max-survivors: "0"          # any survivor blocks
          comment: "true"
          metrics-file: "slop-gate-metrics.jsonl"
```

The Action resolves the merge-base with `git merge-base`, installs a prebuilt
musl binary (building from source only if the release is missing), and runs the
gate against the PR diff.

**Private action repo.** The binary is fetched via the authenticated GitHub
*asset* API, so a **private** action repo works too — pass a read-scoped PAT or
App token as `release-token` (the consumer repo's default `GITHUB_TOKEN` can't
read a *different* private repo's releases). The same token authenticates the
source-build fallback (via a git `insteadOf` rewrite, so it never reaches a log
line). Leave `release-token` unset for a public action repo. If you gate the
*same* private repo that holds the gate, you don't need any of this — build from
source in CI instead (see §6).

```yaml
      - uses: your-org/private-rust-project/.@slop-gate-v0.4.0
        with:
          release-token: ${{ secrets.SLOP_GATE_READ_TOKEN }}   # read access to the action repo
          max-survivors: "0"
```

### 2. Roll out advisory → blocking

Don't start blocking on day one. Recommended sequence:

1. **Advisory first.** Set `advisory: "true"`. The gate reports survivors as a PR
   comment but never fails. Watch a dozen PRs; confirm survivors are real and
   latency is acceptable.
2. **Blocking, lenient.** Drop `advisory`, set a non-zero `max-survivors` or
   `block-on-severity: "high"` so only the dangerous misses block.
3. **Blocking, strict.** Tighten to `max-survivors: "0"` once the team trusts it.

### 3. Make it required

Branch protection → require the `behavioral-gate` check. Only then does a blocked
verdict actually prevent merge. The gate is idempotent and merge-queue safe.

### 4. Action inputs (common)

| Input | Default | Purpose |
| ----- | ------- | ------- |
| `max-survivors` | `0` | Survivors tolerated before blocking. |
| `advisory` | `false` | Report but never block. |
| `block-on-severity` | _(unset)_ | Block at/above a tier (`low`…`critical`). |
| `max-per-function` | `5` | Cap mutants per function — the main cost lever. |
| `block-on-zero-assertion` | `false` | Also block on assertion-free tests. |
| `debt-budget` / `block-on-debt` | `25` / `false` | Debt-delta budget + gating. |
| `block-on-pattern` | _(unset)_ | Gate a pattern lane: a lane (`slop`/`security`/`convention`/`all`) or rule id (`hardcoded-secret`, `unknown-crate-import`, …). Comma-separated. Advisory if unset. |
| `metrics-file` | _(unset)_ | Append a JSON-Lines telemetry record. |
| `enable-python` | `false` | Install Python + cosmic-ray so changed `.py` files are mutation-tested (advisory). |
| `comment` | `true` | Post/update the PR comment. |
| `release-token` | _(unset)_ | Read-scoped PAT/App token to fetch the prebuilt binary when the **action repo is private** (see below). |

### 5. Latency budget & tuning

Cost ≈ baseline build + (mutant count × per-mutant test time). The orchestrator
is ~11 ms (see [`BENCHMARK.md`](./BENCHMARK.md)); **>90% of wall-clock is the
engine rebuilding + re-running the suite once per mutant.** So tuning is about
making each rebuild cheap, reusing the baseline, and parallelising — in ROI
order:

> **Workspace scoping (automatic, correctness-preserving).** In a Cargo workspace
> the gate resolves the package(s) the changed files belong to and passes
> `--package` so it only *mutates* the changed crate — but by default it still
> runs the **whole workspace's tests** (`--test-workspace`) against each mutant,
> so a mutant caught only by a *downstream* crate's tests is still caught (no false
> survivors). Set `test-changed-package-only` (CLI `--test-changed-package-only`)
> to narrow tests to the changed crate for more speed, accepting that a
> downstream-only catch then reads as a survivor.

1. **Do NOT set `CARGO_INCREMENTAL=0`.** cargo-mutants depends on incremental
   compilation to rebuild only the mutated crate between mutants; disabling it
   forces a near-full recompile *per mutant* — often the single biggest
   slowdown. Leave it unset (or `=1`). It's a common copied-CI default that's
   wrong for mutation testing.
2. **`Swatinem/rust-cache@v2`** before the gate step — warms `target/` so the
   baseline build isn't paid cold each run.
3. **`--skip-preflight`** (when CI already ran your suite green): the gate then
   passes `--baseline skip` to cargo-mutants, dropping a whole baseline
   build+test cycle.
4. **`mold` linker + `sccache`** — link/compile dominate incremental rebuilds;
   both cut real time. `RUSTFLAGS="-C link-arg=-fuse-ld=mold"` + `sccache` as
   `RUSTC_WRAPPER`.
5. **`test-tool: nextest`** — run the suite with `cargo-nextest` (per-process,
   highly parallel) instead of `cargo test`; often a 2–3× faster *test* phase.
   Set `test-tool: "nextest"` (CLI `--test-tool nextest`, action input); needs
   `cargo-nextest` installed.
6. **Parallelism** — `jobs` defaults to `min(cores, 8)`; a 2-core runner runs
   mutants nearly serially. Use a larger runner (more cores) or shard across
   runners for big diffs.
7. **`max-per-function`** caps mutants per function (the count driver). `--in-diff`
   already scopes mutation to changed lines.

> **Test selection.** Re-running the *whole* suite per mutant is the floor once
> compilation is cheap. The **JS engine** prunes this automatically — Stryker runs
> with `coverageAnalysis: perTest`, so only the tests that cover a mutant run
> against it. `cargo`/`cargo test` has no native test-impact analysis;
> `nextest` (above) speeds the full run but doesn't select. Coverage-guided
> per-test selection for Rust is a future lever (needs per-test coverage maps).

A worked example: a 25-mutant run at ~316 s on a 2-core runner with
`CARGO_INCREMENTAL=0` and a cold `target/` typically drops to ~1–2 min after
(1)+(2)+(3), and further on a bigger runner.

Exit codes: `0` pass/advisory · `2` blocked (fails the check) · `1` operational
failure (couldn't diff, engine missing, etc.).

### 6. Private builds — where the binary comes from

The composite Action (§1) fetches a **prebuilt musl binary** from a public
GitHub Release (`slop-gate-release.yml`), falling back to a source build. That's
the *cross-repo public* path. For a **private** setup, pick by who consumes it:

| Your setup | How to build/get the binary | Auth needed |
| ---------- | --------------------------- | ----------- |
| **Same private monorepo** gating its own PRs | **Build from source in CI** — `cargo build --bin slop-gate --release`, then run `./target/release/slop-gate`. The binary is an in-CI artifact, never published. See [`.github/workflows/slop-gate-selfgate.yml`](../../../.github/workflows/slop-gate-selfgate.yml). | none |
| **Other private repos** consume it | Build once → upload the musl tarball as a **private Release asset** (keep `slop-gate-release.yml`), and download it in the Action via the authenticated GitHub *asset* API (`Authorization: Bearer $TOKEN`, `Accept: application/octet-stream`), not the public URL. Or publish a **private GHCR image** and pull it. | Release asset: `repo` (classic PAT) or **Contents: read** (fine-grained PAT). GHCR: `read:packages` (classic PAT) + `packages: read` job permission. **`GITHUB_TOKEN` only pulls same-repo** — cross-repo GHCR needs a PAT. |
| **Air-gapped / self-hosted** | Build in an internal pipeline → push to S3/Artifactory or bake into the self-hosted runner image; the step just `cp`s it onto `PATH`. | internal store creds |

Recommended for this repo: **build from source** (above). It needs no release
pipeline, no registry, and no token — the cost is a ~30–45 s compile per cold run,
amortised by `Swatinem/rust-cache`. Keep the binary build private simply by keeping
the repo private; nothing is pushed outward.

> The cross-repo paths exist because the Action defaults to a *public* release
> download — switch that to the authenticated asset API (or a source build with a
> token) before pointing a private consumer at it, or the fetch will 404.

---

## Developer integration

### Install the CLI

```bash
cargo install cargo-mutants                       # the mutation engine
cargo build --release                # binary: target/release/slop-gate
```

> **Platform:** run the mutation step on **Linux** (the GitHub Action uses
> `ubuntu-latest`). On **Windows**, `cargo-mutants` currently fails with a
> recursive temp-path error (it nests its build directory past `MAX_PATH`), so
> the mutation step can't complete. The rest of the gate (diff, zero-assertion,
> debt-delta, `analyze`) works on any platform; for an end-to-end Windows run use
> the closed-environment container — see [Test it in a closed environment](#test-it-in-a-closed-environment-first).

### Run it on your branch before pushing

```bash
# Blocking gate vs origin/main (exit 2 if it would block)
slop-gate --repo . --base origin/main --head HEAD

# Just look, don't block
slop-gate --base origin/main --advisory

# CI already ran the suite green this run — skip the pre-flight
slop-gate --base origin/main --skip-preflight
```

A natural pre-push hook (`.git/hooks/pre-push`):

```bash
#!/usr/bin/env bash
slop-gate --base origin/main --advisory || true   # advisory locally; CI enforces
```

### Predict the cost first — `estimate` (dry run)

Want to know how many mutants a diff will generate — and roughly how long the gate
will take — *before* paying the build/test cycle? `slop-gate estimate` runs only the
enumerate + per-function-cap steps (`cargo mutants --list`, no build):

```bash
slop-gate estimate --repo . --base origin/main --head HEAD
# ── Slop Filter · mutant estimate (dry run, no build) ──
# function                                   line candidates test   capped
# arith (src/lib.rs)                            2          5    5        -
# opt (src/lib.rs)                              3          5    5        -
# cmp (src/lib.rs)                              1          3    3        -
#
# total: 13 candidate(s) → 13 tested (0 capped at 5/fn across 3 group(s))
# est. mutation time: ~0.4s (warm target/) … ~6.5s (cold build)
```

- **candidates** = mutants the enumerator found on the changed lines; **test** =
  what would actually run after the per-function cap; **capped** = the surplus.
- `--max-per-function N` previews a tighter cap; `--json` emits a machine record
  for CI dashboards.
- It's the **cross-platform** path: `--list` works on Windows even though a full
  mutation run there can't (see the platform note above). Mutant counts per
  construct are explained in [`BENCHMARK.md`](./BENCHMARK.md).

### Reading the report

```text
outcomes:   4 caught · 0 timed out · 0 unviable · 1 SURVIVED
verdict:    BLOCK — 1 surviving mutation(s) exceed the allowed 0

Survivors — the suite passed over these mutations (most severe first):
  • [high] src/auth.rs:42:9  replace >= with > in can_access
```

Each survivor is a real edit your tests didn't notice, at `file:line`. Read it as
"if someone made *this* change, your suite would stay green." `[high]` is the
severity tier (most dangerous first).

### Fix vs. override

- **Fix (preferred):** add/strengthen an assertion so the mutant is caught. The
  survivor disappears on the next run — telemetry records it as *fixed*.
- **Override:** if it's a genuine false positive (e.g. a log-string mutation you
  don't care about), raise `max-survivors`, exclude the path, or lower the
  severity gate. Telemetry records a *carried* survivor so overrides stay visible
  rather than silent.

### Tuning

All flags also live in a YAML config (`--config slop-gate.yaml`); CLI flags
override the file, the file overrides defaults. Start from
[`mergestro-gate.example.yaml`](./mergestro-gate.example.yaml).

### Look at the trend

```bash
slop-gate analyze --metrics-file slop-gate-metrics.jsonl
# block rate, fix-vs-override, latency, mutation-score trend, severity mix
```

---

## Test it in a closed environment first

To see the whole pipeline run **offline**, with a known survivor, before wiring
it into CI, use the network-isolated harness:

```bash
cd test-harness
podman compose -f podman-compose.yml build        # needs network (one-time)
podman compose -f podman-compose.yml run --rm gate # closed: network_mode none
```

See [`test-harness/README.md`](./test-harness/README.md) for what each scenario
demonstrates and how to point it at your own crate.
