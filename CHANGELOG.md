# Changelog

All notable changes to Mergestro Gate (`slop-gate`) are documented here. The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the
project aims to follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- **Progression trees** (`src/progression/`, `slop-gate progression`) — an
  authored milestone tree (`progression.yaml`) resolved against the repository's
  own commits and PRs. Each part declares an evidence rule (path globs, a subject
  regex, a commit or distinct-PR threshold, or `manual`), and a milestone closes
  only when every part closed **and** every `requires` is done. Renders the
  committed `docs/progression.svg`, the README block between the
  `mergestro:progression` markers, a JSON snapshot and a job-log summary;
  `--check` turns the first two into a CI assertion (exit 2 when stale).
  `--plane-url` uploads a `record_type: "progression"` snapshot, which Mergestro
  draws on its Progression canvas. A shallow history is refused rather than
  silently understating every milestone. Reference:
  [`docs/PROGRESSION.md`](docs/PROGRESSION.md).
- **`slop-gate progression init`** (`src/progression/init.rs`) — scaffolds a
  first plan from the repository's own history: the directories that carry work
  (counted once per commit, build output and lockfiles skipped, containers split
  and wrapper directories descended through) and the merge marker the repository
  actually writes, which are the two things a hand-written first spec gets wrong
  *silently*. Milestones are ordered by when each component first appeared and
  grouped into phases — only when the walk saw the whole history, since inside a
  window everything "first appeared" at its edge. Because history can describe
  what a repository has done but never what it meant to do, every mined milestone
  is already closed and the draft ends with one open frontier milestone for the
  author to replace; the command prints what the draft resolves to so the
  retro-fit is stated rather than discovered. Under two components it writes a
  starter plan instead, which on a young repository resolves near zero and fills
  in as the work lands. The YAML is commented, and is parsed back and validated
  before it is written.
- **`examples/progression/`** — the authoring step worked through: `try-it.sh`
  runs init → resolve → render on a throwaway repository, `specs/` holds three
  finished plans (a young service, an established repo adopting one, a two-week
  sprint), and `prompts/` has paste-ready prompts for authoring a plan with an
  assistant. The prompts are mostly defence against the three failures a model
  reliably produces here — inventing paths that do not exist (silent: an
  unmatched glob renders exactly like an unstarted milestone), describing
  finished work instead of planned work, and writing milestones nothing can
  falsify — and they require the model to declare every glob it could not
  verify. The worked specs are loaded and validated by the test suite so they
  cannot rot.
- **Turnover lane** (`src/turnover_lane.rs`) — the longitudinal maintainability
  gate, in-process. Refreshes a per-commit baseline of the repository's history
  (`.turnover/baseline.json`) with the commits the PR adds, then judges those
  commits' copy/paste, duplicated-block, refactor and churn ratios against the
  repository's **own** baseline under a drift policy. Advisory by default; a
  `fail` becomes a block reason with `turnover.block_on_drift` (CLI
  `--block-on-turnover-drift`, Action `block-on-turnover-drift`). A missing
  baseline is a *skipped* lane with a hint, never a block. New `slop-gate
  baseline` subcommand builds/refreshes the file; the Action builds it on first
  run (needs `fetch-depth: 0`). Config: the `turnover:` block (`enabled`,
  `baseline`, `block_on_drift`, `refresh`, `update_baseline`, plus the turnover
  policy keys `gate` / `thresholds` / `drift` / `signals` / `churn` / `walk`
  verbatim). The PR comment gains a "Maintainability drift" section; the run
  record gains a `turnover` summary and the lane's own `record_type: "turnover"`
  record travels as a second JSON line on the same `metrics_url` POST, which
  Mergestro turns into `/v1/fleet/turnover`. Duplicated blocks match
  rename-insensitively (identifiers and literals abstracted, token floor), the
  report names each block's twin and lists the worst files, and every window is
  split AI-coauthored vs human from commit trailers and bot identities.
  `turnover.baseline_url` (CLI `--turnover-baseline-url`, Action
  `turnover-baseline-url`) fetches the baseline from the Mergestro plane's
  baseline service before a run and pushes it back after `slop-gate baseline`.
- **MCP lane** (`src/mcp_gate.rs`) — fail the PR when this repository's own MCP
  server breaks. On a diff touching a declared server the gate builds it, spawns
  it over stdio via the `specprobe` prober, and blocks the merge on a
  conformance regression, naming the failing check in the PR comment it already
  posts. Config: `mcp_servers[]` (`name`, `paths`, `command`, optional `build` /
  `dir` / `spec_version` / `timeout_secs` / `elicit_tool`), `mcp_fail_on`
  (`never` | `critical` | `unproven` | `any`, default `critical`) and
  `specprobe_bin`; CLI `--mcp-fail-on` / `--specprobe-bin`. Declaring a server is
  the opt-in, so the lane gates from the first run; `--advisory` turns it
  advisory with the rest, and an explicit `--mcp-fail-on` is narrower and wins.

  Three properties, all tested: a **skipped** check never blocks at any
  threshold (over stdio roughly a third of the check catalog cannot be expressed
  at all); a lane that **could not run** — missing prober, failing build,
  unreadable output — blocks rather than contributing nothing; and the gate
  carries no copy of the check catalog, passing the threshold down to the prober
  and reading the failing ids back, so it cannot drift out of date in the
  direction of passing.
- **Liveness in the MCP lane** — when the probed server stops answering, that is now the first
  thing the PR comment says and the lead of the block reason, rather than a pair of check ids the
  reader has to translate. A clean run adds no headline, and a prober too old to report liveness
  adds nothing at all: absent means nobody measured, not that the server stayed up.
- `DiffScope::all_changed_files` — every path a diff touches, deletions and any
  extension included. The unified diff is a `cargo-mutants` artifact carrying
  only mutable files, so a server whose tool schema or Dockerfile changed looked
  untouched to a path-scoped lane.
- `CommandRunner::run_env` — subprocess execution with extra environment
  variables. Required rather than defaulted on purpose: the prober is configured
  entirely through the environment, so a runner that silently dropped it would
  probe nothing and report a clean run.

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
