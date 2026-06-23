# mancube/mergestro-gate

The **Mergestro Gate** CLI (`slop-gate`) — a behavioral merge gate for
AI-generated code. Alpine-based, multi-arch (amd64 + arm64), runs as nonroot,
with the Rust toolchain + `cargo-mutants` baked in so the **mutation gate runs
fully in-container**.

## Tags

- `latest`, `0.1.0`, `0.1` — `slop-gate` + Rust toolchain + `cargo-mutants` on Alpine.

## What's included

- **`slop-gate`** — the gate CLI.
- **`cargo` + `rustc`** (Alpine `rust:1-alpine` base) and **`cargo-mutants`** —
  the Rust mutation engine; the behavioral gate compiles + tests each mutant, so
  it needs a toolchain at run time. It's baked in.
- **`git`** — for merge-base / diff against the base ref.

The non-Rust engines (Python/JS/Go/JVM) are advisory PoCs and are **not** bundled
— install their toolchains yourself if needed.

## Quick start

```bash
# Run the behavioral gate over the working tree's diff vs origin/main:
docker run --rm -v "$PWD:/work" mancube/mergestro-gate:latest \
  --repo /work --base origin/main

# Predict the mutant workload without building/testing (fast):
docker run --rm -v "$PWD:/work" mancube/mergestro-gate:latest \
  estimate --repo /work --base origin/main

# Summarise telemetry from a prior run:
docker run --rm -v "$PWD:/work" mancube/mergestro-gate:latest \
  analyze --metrics-file /work/slop-gate-metrics.jsonl

# Help / version:
docker run --rm mancube/mergestro-gate:latest --help
```

The container's working directory is `/work`; mount your repo there. The repo
needs to be a git checkout with the base ref available (e.g. fetch `origin/main`).

## Note

For CI, the [GitHub Action](https://github.com/lucheeseng827/mergestro-gate) is
usually simpler than wiring this image into a job. The image is for local runs,
non-GitHub CI, and as a self-contained gate environment.

## Links

- Source & docs: https://github.com/lucheeseng827/mergestro-gate
- License: Apache-2.0
