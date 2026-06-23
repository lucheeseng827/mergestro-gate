# Mergestro Gate (`mancube/mergestro-gate`)

**A behavioral merge gate for AI-generated code — it mutation-tests your diff and blocks merges whose new code isn't actually covered by tests.**

LLM-written code compiles, looks plausible, and ships with tests that assert
nothing. Mergestro Gate runs **differential mutation testing** on the changed
lines: it mutates your diff, reruns the suite, and any mutant that survives is a
line your tests don't really check. It also runs static **slop**, **security**,
and **convention** lanes over the same diff. One verdict, gate-able in CI.

- **Image:** `mancube/mergestro-gate` — `slop-gate` CLI on **Alpine** (`rust:1-alpine`), with the Rust toolchain + **`cargo-mutants`** baked in, so the mutation gate runs **fully in-container** (no toolchain to wire up).
- **Arch:** `linux/amd64` (arm64 buildable on demand — see *Multi-arch* below) · **Runs as:** nonroot (uid 65532)
- **Binary inside:** `/usr/local/bin/slop-gate` (entrypoint) · **Workdir / mount point:** `/work`
- **Source / full docs:** github.com/lucheeseng827/mergestro-gate · Apache-2.0

## Tags

| Tag | Notes |
|---|---|
| `latest` | newest release |
| `0.1.0` | pinned version (= current `latest`) |
| `0.1`   | latest `0.1.x` |

Pin a version in CI: `mancube/mergestro-gate:0.1.0`.

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
docker run --rm -v "$PWD:/work" mancube/mergestro-gate:0.1.0 \
  --repo /work --base origin/main

# Predict the mutant workload first, without building/testing (fast):
docker run --rm -v "$PWD:/work" mancube/mergestro-gate:0.1.0 \
  estimate --repo /work --base origin/main

# Summarise telemetry from prior runs (--metrics-file output):
docker run --rm -v "$PWD:/work" mancube/mergestro-gate:0.1.0 \
  analyze --metrics-file /work/slop-gate-metrics.jsonl

# Help / version:
docker run --rm mancube/mergestro-gate:0.1.0 --help
```

> The gate compiles + tests every mutant in your diff, so wall-clock scales with
> diff size × suite time. Use `estimate` to size a run before committing to it.

## As a CLI base layer

Prefer your own toolchained image? Copy the binary in and bring your own
`cargo-mutants`:

```dockerfile
FROM rust:1-bookworm
COPY --from=mancube/mergestro-gate:0.1.0 /usr/local/bin/slop-gate /usr/local/bin/slop-gate
RUN cargo install cargo-mutants
ENTRYPOINT ["slop-gate"]
```

## Multi-arch

Published images are `linux/amd64`. An `arm64` build exists but is opt-in — the
arm64 leg compiles `cargo-mutants` under QEMU emulation (slow), so it's run on
demand via the repo's `Publish Docker image` workflow rather than every release.

## In CI

For GitHub Actions, the
[Mergestro Gate Action](https://github.com/lucheeseng827/mergestro-gate) is
simpler than wiring this image into a job (it runs on a toolchained runner). Use
this image for local runs, non-GitHub CI, and as a self-contained gate
environment.

## Links

- **Source & docs:** https://github.com/lucheeseng827/mergestro-gate
- **License:** Apache-2.0
