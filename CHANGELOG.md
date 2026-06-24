# Changelog

All notable changes to Mergestro Gate (`slop-gate`) are documented here. The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the
project aims to follow [Semantic Versioning](https://semver.org/).

## [0.1.1] - 2026-06-24

First public release. (Supersedes the `v0.1.0` tag, which was consumed during
setup and not published to the Marketplace.)

### Added
- Behavioral merge gate: diff-scoped mutation testing with a blocking verdict,
  severity ranking, debt-delta budget, zero-assertion pre-check, idempotent PR
  comment, validation telemetry + `analyze`.
- Language engines behind a `MutationEngine` trait: **Rust** (cargo-mutants,
  stable) + **Python** (cosmic-ray), **JS/TS** (Stryker), **Go** (gremlins),
  **Java/Kotlin** (PIT) as experimental PoCs.
- Pattern lanes (static, advisory): slop signatures, security anti-patterns,
  convention / hallucinated-import.
- Opt-in gating: `--block-on-severity`, `--block-on-debt`, `--block-on-pattern`.
- `estimate` dry-run (mutant count + latency, no build).
- Performance levers: `--package` mutation scoping with `--test-workspace`
  default, `--baseline skip` on `--skip-preflight`, `--test-tool nextest`.
- Composite GitHub Action with public + authenticated-private binary fetch.

> Pre-1.0: interfaces may change.
