# Contributing to Mergestro Gate

Thanks for helping! This is the Apache-2.0 **open core** of the behavioral merge
gate. (The paid control plane under `ee/` is source-available under a separate
license and isn't part of OSS contributions.)

## Ground rules

- By contributing you agree your work is licensed under **Apache-2.0** (the
  project's inbound = outbound license). No CLA.
- Be civil — see [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md).
- Security issues go through [`SECURITY.md`](./SECURITY.md), **not** public issues.

## Dev setup

```bash
cargo build            # the slop-gate binary
cargo test             # unit tests (no external engine needed)
cargo clippy --all-targets
cargo fmt --all
```

The mutation engines are external tools you only need to *run* the gate
end-to-end (Rust: `cargo install cargo-mutants`; the Python/JS/Go/JVM engines
are experimental — see the README). The unit tests mock the command runner, so
they don't require any engine installed.

## Before you open a PR

- `cargo fmt --all --check`, `cargo clippy --all-targets`, and `cargo test` all
  pass.
- Add/adjust tests for behavior changes — this is a *test-quality* tool; we hold
  ourselves to it. New parsing/logic gets unit tests.
- Keep the diff focused; one concern per PR. Match the surrounding style and
  comment density.
- Update the docs you touched (README / GUIDE / ACTION).

## Adding a language engine

Implement `MutationEngine` (`src/engine.rs`) for the new tool and register it in
`default_engines()`. Mirror an existing adapter (`golang.rs` / `js.rs` / `jvm.rs`):
detect tool, run it diff-scoped, parse its report, filter to the changed lines,
map outcomes to caught / survived / timed-out / unviable. Add parser unit tests.

## Scope

This repo is the gate. Cross-repo dashboards, policy, managed AI, and the agent
feedback loop are the commercial layer and live elsewhere — PRs here should keep
the gate self-contained and runnable entirely inside a single CI job.
