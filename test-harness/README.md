# Closed-environment test harness

Run the Slop Filter behavioral merge gate end-to-end in a **network-isolated**
container, against the bundled theater-test fixture. This proves the whole
pipeline — `gix` diff → determinism pre-flight → `cargo-mutants` → verdict,
severity ranking, debt-delta, telemetry — works offline, exactly as it would
inside a customer's CI.

## Why a container

The gate shells out to `cargo-mutants`, which compiles and re-tests the target
crate once per mutant. The image bakes in the Rust toolchain, `cargo-mutants`,
and the `slop-gate` binary so a run needs **no network** — the demo crate has
zero dependencies, so mutation testing happens entirely closed.

## Run it

With Podman:

```bash
cd ./test-harness
podman compose -f podman-compose.yml build          # needs network (fetches crates)
podman compose -f podman-compose.yml run --rm gate   # network_mode: none — closed
```

The same compose file works with Docker (`docker compose -f podman-compose.yml …`).

Prefer raw Podman without compose? Equivalent:

```bash
podman build -f test-harness/Containerfile -t slop-gate-harness ..
podman run --rm --network none slop-gate-harness
```

## What the demo does

`entrypoint.sh` builds a throwaway git repo from
[`fixtures/theater_demo`](../fixtures/theater_demo): a base commit with
`is_adult(age) = age > 17`, then a head commit rewriting the boundary as
`age >= 18`. The theater test exercises `is_adult` but never asserts the
boundary, so a `>=`→`>` mutation survives. The gate is then run several ways:

| Run | Flags | Expected |
| --- | ----- | -------- |
| 1 | _(default, blocking)_ | Surfaces the HIGH-severity survivor → **exit 2** |
| 2 | `--advisory` | Same report, never blocks → **exit 0** |
| 3 | `--max-survivors 9 --block-on-severity critical` | Survivor is HIGH < critical → **exit 0** |
| 4 | `--max-survivors 9 --block-on-severity high` | Survivor meets the tier → **exit 2** |
| 5 | `analyze --metrics-file …` | Prints the validation + trend dashboard |

Exit code `0` = passed/advisory, `2` = blocked, `1` = operational failure.

## Customise

Edit [`entrypoint.sh`](./entrypoint.sh) to point the gate at your own crate
(`--repo`, `--base`, `--head`) or to tune thresholds (`--debt-budget`,
`--block-on-debt`, `--max-per-function`). To gate a real project closed, mount
it read-only and pass a config file:

```bash
podman run --rm --network none \
  -v "$PWD/my-crate:/work/repo:ro" \
  --entrypoint slop-gate slop-gate-harness \
  --repo /work/repo --base origin/main --head HEAD
```
