# COGS baseline harness

Measures the **cost of goods** for the behavioral merge gate: the gate shells out
to `cargo-mutants`, which compiles and re-tests the crate once per mutant, so the
runtime (and the CI bill) is dominated by that compile-and-test cycle — not by the
Rust orchestrator. This harness quantifies exactly that, reproducibly and offline.

It answers three questions:

1. **Orchestration tax** — wall-clock `slop-gate` adds *on top of* the raw
   `cargo-mutants` engine (gix diff + per-function cap + parse + verdict). The
   thesis is this is negligible; the bench proves it with a number.
2. **Cost breakdown & scaling** — enumerate vs build+test vs orchestrate, and how
   each grows with diff size / mutant count. Tells you *where* to optimize.
3. **Cross-language anchor** — per-mutant wall-clock for the same logical change
   under `cargo-mutants` (Rust) vs `mutmut` (Python) vs `Stryker` (JS).

> **LLM reviewers** (CodeRabbit et al.) are **not** measured here — the gate runs
> no model. They appear as a documented reference row in
> [`../BENCHMARK.md`](../BENCHMARK.md), since the signal (and cost basis: tokens,
> not CI minutes) is different in kind.

## Run it

```bash
cd .
podman build -f bench/Containerfile -t slop-gate-bench .      # network: toolchains
podman run --rm --network none \
  -v "$PWD/bench/out:/work/out" slop-gate-bench                # offline measure
```

Results: a human table on stdout + machine-readable `bench/out/results.json`.

### Knobs (env, `-e NAME=value`)

| Var | Default | Meaning |
| --- | --- | --- |
| `ITERS` | `5` | iterations per measurement; **median** reported |
| `RUST_SIZES` | `1 5 15` | function counts (≈ 3× mutants each) for the scaling sweep |
| `XLANG` | `1` | also run the Python/JS per-mutant anchor |
| `JOBS` | `nproc` | `cargo-mutants --jobs` parallelism |
| `TIMEOUT` | `60` | per-mutant test timeout (s) |

Quick smoke run: `-e ITERS=1 -e RUST_SIZES=1 -e XLANG=0`.

## Reading the table

```
funcs  mutants  list      raw       orch      gate        overhead
1      3        0.4s      2.1s      0.05s     2.55s       18.0%
```

- **list** — `cargo-mutants --in-diff --list` (enumeration only, no build).
- **raw** — `cargo-mutants` full run: the engine floor, ~all of it build+test.
- **orch** — pure wrapper cost = `gate − raw − list` (diff/cap/parse/verdict).
- **gate** — `slop-gate` end-to-end.
- **overhead** — `(gate − raw) / gate`: everything the gate does beyond the engine
  run, including the enumeration pass it needs for the per-function cap.

`orch` is the number to watch: if it stays flat and tiny as `mutants` grows, the
optimization headroom is in the **engine** (baseline build reuse, `sccache`,
`mold`, sharding), not the orchestrator — confirming the build-plan's COGS thesis.

## Platform

Linux only — `cargo-mutants` can't run on Windows (recursive temp-path > `MAX_PATH`).
The container pins that; run it on any host with Podman/Docker.
