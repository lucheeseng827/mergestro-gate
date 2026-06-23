# COGS baseline — Slop Filter behavioral merge gate

**What this answers:** the build plan says the cost center is the *compile-and-test
cycle per mutant*, not the Rust orchestrator, so optimization effort should target
mutant count / baseline build / parallelism — **not** the orchestrator. This is the
measurement that backs that claim, plus a cross-engine anchor and the headroom map.

Reproduce with [`bench/`](./bench) (`podman build … && podman run --network none …`).
Numbers below: **median of 7 iterations**, warm `target/`, offline, inside the
bench container on a 32-core Linux host. `cargo-mutants 27.1.0`.

> These are *relative* numbers on a zero-dependency micro-fixture — they isolate
> engine vs orchestrator cleanly but understate real-world baseline-build cost (see
> [Caveats](#caveats)). Treat them as a floor and a ratio, not an absolute SLA.

## 0. How a diff becomes mutants (the cost driver)

Runtime scales with **mutant count**, and mutant count comes from the *constructs
on the changed lines* — not from line count or file size. cargo-mutants scans each
changed line, finds mutable constructs, and emits a fixed set of mutants per
construct; `--in-diff` keeps only those whose line is in the PR diff; then the gate's
per-function cap (`max_mutants_per_function`, default 5) trims the rest.

Measured mapping (`cargo mutants --list` over a fixture of one construct each):

| Construct on the changed line | Mutants | What they are |
| ----------------------------- | ------: | ------------- |
| `bool` return — `a >= b`             | **3** | body → `true`; body → `false`; `>=` → `<` |
| integer return + 2 ops — `a + b * 2` | **7** | return `0` / `1` / `-1`; `+` → `-` / `*`; `*` → `+` / `/` |
| logic — `x && y`                     | **3** | → `true`; → `false`; `&&` → `\|\|` |
| unit fn w/ `+=` — `*v += 1`          | **3** | body → `()`; `+=` → `-=` / `*=` |
| `Option<i32>` + compare              | **7** | return `None` / `Some(0)` / `Some(1)` / `Some(-1)`; `>` → `==` / `<` / `>=` |
| `match` → `u8`                       | **3** | return `0`; return `1`; delete arm |

What pushes the count up:

1. **Return-type richness** — `bool` → 2 value replacements; integer → 3 (`0,1,-1`);
   `Option<T>` → 4. Richer return type = more mutants.
2. **Operators on the line** — each binary op (`>`, `+`, `&&`, `+=`) → ~1–2 swaps.
3. **Match arms** — each arm → one "delete arm" mutant.

Per changed function, before the cap:

```
mutants ≈ (return-value replacements) + (operator swaps) + (arm/body deletions)
```

Two things bound it for a real PR:

- **`--in-diff` is line-scoped** — changing one line inside a 200-line function
  mutates only that line's constructs, not the whole function.
- **Per-function cap (default 5)** — a dense function is trimmed to the cap; the
  surplus is excluded via anchored `--exclude-re`. So:

```
PR mutants ≈ Σ over touched functions of  min(cap, raw_mutants_on_changed_lines)
```

| Typical PR change | ≈ mutants (cap 5) |
| ----------------- | ----------------: |
| one boundary/comparison fix | 3 |
| a 5-line bool/arith helper  | 10–15 |
| 5 dense functions touched   | ~25 (5 × cap) |

**Preview any diff without building:** `slop-gate estimate --base <ref> --head HEAD`
prints this projection (per-function candidates → tested → capped) plus a latency
band — see [`GUIDE.md`](./GUIDE.md#predict-the-cost-first--estimate-dry-run).

## 1. Orchestration tax — what slop-gate adds over the raw engine

| Δfuncs | mutants | list (enumerate) | raw (cargo-mutants) | **orch (pure wrapper)** | gate (total) | overhead |
| -----: | ------: | ---------------: | ------------------: | ----------------------: | -----------: | -------: |
| 1      | 3       | 0.033s           | 0.340s              | **0.011s**              | 0.384s       | 11.5%    |
| 5      | 15      | 0.035s           | 0.499s              | **0.012s**              | 0.546s       | 8.6%     |
| 15     | 45      | 0.036s           | 0.986s              | **0.011s**              | 1.033s       | 4.5%     |

- **orch** = `gate − raw − list`: the gix diff + per-function cap + outcome parse +
  verdict + process spawn. It is **flat at ~11 ms** regardless of mutant count — the
  Rust orchestrator is O(diff size), not O(mutants), and the diff here is constant.
- **list** (the enumeration pass the cap needs) is also flat (~34 ms) — itself a
  cargo-mutants call, but list-only, so no build.
- **raw** grows with mutant count (each mutant = one compile + test) and is ~90%+ of
  wall-clock by 45 mutants. The `overhead` column falls (11.5% → 4.5%) purely because
  the fixed wrapper cost is amortized over more engine work.

**Conclusion:** the orchestrator is not on the cost curve. On a realistic diff (tens
of mutants) it is <5% and shrinking. **The COGS thesis holds — spend optimization on
the engine, not the wrapper.**

## 2. Cross-language per-mutant cost (same logical change, 5 functions)

| Engine | Lang | mutants | total | **per-mutant** | rel. |
| ------ | ---- | ------: | ----: | -------------: | ---: |
| cargo-mutants | Rust   | 15 | 0.498s | **0.033s** | 1.0× |
| mutmut        | Python | 10 | 0.465s | **0.046s** | 1.4× |
| Stryker       | JS     | 25 | 2.483s | **0.099s** | 3.0× |

Different engines emit different operator sets (hence different mutant counts), so
**per-mutant wall-clock** is the comparable figure. cargo-mutants is fastest per
mutant here; Stryker carries a large fixed Node/framework startup that dominates at
this scale. This is a *like-for-like micro-anchor*, not a language verdict — on a
real Rust crate the per-mutant cost rises with baseline build time (§Caveats), which
is exactly the lever §3 targets.

## 3. LLM PR-reviewer reference (not measured — different cost basis)

The gate runs **no model** ([grep confirms](./src): no LLM client, no API key). An
LLM reviewer (CodeRabbit et al.) is a *different signal* (reads a diff vs *runs* it)
on a *different cost basis* (**tokens**, not CI minutes), so it is a reference, not a
measured row:

| Approach | Unit cost | Latency basis | Catches behavioral gaps? |
| -------- | --------- | ------------- | ------------------------ |
| Slop gate (this) | CI minutes (compile+test/mutant) | seconds, scales w/ diff mutants | **yes — by execution** |
| LLM reviewer | tokens/PR | seconds–minutes/PR (model RTT) | only what it can infer statically |

The gate's cost is **deterministic and diff-scoped**; an LLM judge, if added (Phase 5,
last stage, survivors only), rides on top — it never replaces the measured floor.

## 4. Optimization headroom (where to spend, in priority order)

Because §1 puts ~90% of wall-clock in **raw engine = baseline build + per-mutant
re-test**, that is the only place worth optimizing. In rough ROI order:

1. **Baseline build reuse** — warm/persisted `target/`, `sccache`, `mold` linker.
   The cold baseline build (excluded here via warm `target/`) is the single biggest
   real-world cost; on a dependency-heavy crate it dwarfs everything below.
2. **Fewer mutants** — `--in-diff` (already on) + the per-function cap (already on).
   Tightening the cap or smarter candidate selection cuts the count linearly.
3. **Parallelism** — `--jobs` (already defaults to cores) + `--shard k/n` across
   runners for large diffs; near-linear until the box is saturated.
4. **Orchestrator** — **do not bother.** 11 ms flat. Any work here is noise.

A regression check is built in: re-run `bench/` in CI; if `orchestration_s` ever
grows with mutant count, the wrapper picked up an O(mutants) cost and *that* becomes
worth a look.

## Caveats

- **Zero-dependency fixture.** Mutants build in ms, so this isolates engine-vs-wrapper
  cleanly but **understates** real baseline-build cost (deps, codegen). Real per-mutant
  Rust cost is higher and more build-bound — which only *strengthens* the §4 priority
  (build reuse first).
- **Warm `target/`.** Timings exclude the cold baseline build by design (steady-state
  CI). The cold build is the dominant first-run cost; budget for it separately.
- **Cross-language is an anchor, not a ranking.** Different codebases, operators, and
  runtimes; fixed startup (esp. Stryker) skews small-N. Don't over-read 3.0×.
- **Platform: Linux only.** `cargo-mutants` can't run on Windows (recursive temp-path
  > `MAX_PATH`); the harness is containerized to pin the environment.
- **Hardware-relative.** 32-core host; `raw`/parallel figures scale with core count.
