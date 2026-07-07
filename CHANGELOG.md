# Changelog

All notable changes to Mergestro Gate (`slop-gate`) are documented here. The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the
project aims to follow [Semantic Versioning](https://semver.org/).

## [0.5.0] - 2026-07-08

### Added
- **`docs` pattern lane** (`src/docs_gate.rs`) — the documentation flow as part
  of the gate: presence checks per touched module (README + quickstart, as-built
  mermaid architecture flowchart, event/call `sequenceDiagram`) and drift checks
  (`docs-stale-config` / `docs-stale-api` when new clap/env or route surface
  lands with no doc file touched in the same module). Whole-diff scoped, any
  language; advisory by default, gates via `--block-on-pattern docs` or a rule
  id, exactly like the other pattern lanes.
- Telemetry **schema v6**: records carry `record_type: "slop"` so a line parses
  directly as a Mergestro `/v1/ingest` envelope; pre-v6 records default to
  `"slop"` on read-back. Metrics POSTs are now time-bounded (5 s).
- Action inputs: `metrics-url` and `metrics-token` (exported as `METRICS_TOKEN`,
  masked) for authenticated Mergestro ingest.

### Fixed
- Docs-drift false positives: doc files never entered the unified diff the gate
  analyses, so the `docs` lane could not see docs moving with the code.

### Note
- Versions 0.2.0–0.4.0 shipped without changelog entries; their changes are in
  the git history. Entries resume as of 0.5.0.

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
