# Changelog

All notable changes to Mergestro Gate (`slop-gate`) are documented here. The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the
project aims to follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.6.0] - 2026-09-26

Fast and actionable: fewer suite runs and a time budget, findings on the line
they are about, and the weakened tests mutation testing cannot see.

### Changed
- **One suite run before the first mutant, not three.**
  `preflight_runs` defaults to `1` (was `2`), and cargo-mutants' own baseline is
  always skipped (`--baseline skip`), since the gate only mutates after a green
  pre-flight. Set `--preflight-runs 2` to keep catching flaky suites.
- **Each mutant runs only the changed crate's tests by default.**
  `test_changed_package_only` now defaults to `true`.
  `--test-workspace` (Action input `test-workspace`) restores the 0.5.x behaviour
  of running the whole workspace's tests per mutant; use it when a crate is
  tested mainly from another, or a downstream-only catch reads as a survivor.
  `--test-changed-package-only` still parses and now changes nothing.

### Added
- **"Since the last run" in the PR comment.** The comment carries the survivors'
  identities (`file|mutation|n`, no line number, so moving code does not reset
  them) in a hidden block; the next run reads it back and reports **new · still
  open · resolved**, marks new survivors 🆕, and lists what was fixed. A run that
  did not re-test everything (suite not green, budget stop) claims nothing
  resolved and carries the earlier survivors forward.
- **`--comment-inline` — survivors as review comments on their line** (Action
  input `comment-inline`; needs `--comment`, GitHub only). Each survivor is
  commented once: the summary records which ones have an inline comment, and
  records them only after the review has posted, so a review that fails (Gitea,
  a token without review permission, a brief outage) is a warning, the summary
  still goes out, and those survivors are tried again next run. Only lines the
  diff shows get a comment — GitHub rejects a whole review for one outside
  them; the rest stay in the summary. Not available on `merge-reports`, which
  has no diff; it refuses the flag.
- **`--shard k/n` and `slop-gate merge-reports`** — split the Rust mutants over a
  CI matrix. Kept mutants are dealt round-robin in listing order (the same split
  in every shard); the other engines run on shard 1 only. `merge-reports
  shard*.json` sums the shards' `--format json` reports, recomputes the verdict
  under its own flags (so `--max-survivors` applies to the total), and prints /
  comments / writes SARIF once. It refuses a missing, duplicated or foreign
  shard: a missing shard is untested mutants, not a clean result. Checked on a
  real run: 3 shards merged equal the unsharded run exactly.
- **`--budget` — a wall-clock limit on the Rust mutation run** (`600`, `90s`,
  `10m`, `1h`; Action input `budget`). cargo-mutants records each mutant as it
  finishes, so at the limit the run is stopped (SIGTERM, then a hard kill after
  15 s) and every finished result counts. The rest are reported as **not
  tested**, never as caught or surviving: a `budget:` line in the job log and a
  notice at the top of the PR comment. Mutants run in source order under a
  budget, so the untested set is the same on a rerun. Untested mutants warn by
  default; `--block-on-budget` makes them block. Not yet most-severe-first:
  cargo-mutants has no way to order a single run, and a second run would pay a
  second cold build.
- **Weakened-test lane** (`weakened-tests`). Mutation testing cannot see a PR
  that only deletes a test or trims its assertions: `--in-diff` mutates changed
  code, and such a PR changes none, so it passed silently. The lane compares
  each changed Rust file's tests at base and head: `test-removed` for a test no
  changed file has any more (a test that moved under the same name is not
  reported), `assertions-reduced` for a test whose `assert*!` / `#[should_panic]`
  count fell. It runs before the language short-circuit, so deleting a whole
  test file is caught too. Advisory; `--block-on-pattern weakened-tests` (or a
  rule id) makes it block. In the report, the PR comment and SARIF.
- **`--sarif <PATH>` — findings for GitHub code scanning** (Action inputs `sarif`,
  `upload-sarif`). Writes SARIF 2.1.0 alongside the normal output: survivors
  (critical/high → error, medium → warning, low → note), zero-assertion tests,
  and the slop / security / convention / docs lanes, each on its file and line.
  No fingerprints of its own: `upload-sarif` computes GitHub's per-line hash,
  which keeps identical mutations in one file distinct (survivors read back from
  cargo-mutants carry no function name to tell them apart). The Action removes
  any old file before the run and uploads only one this run wrote, even when the
  gate blocks.
- **`--in-place` (Action input `in-place`) — no second cold build.** cargo-mutants
  builds mutants in a scratch copy that starts without `target/`, so every run
  paid a cold build after the pre-flight had just built the same tree, and a
  cached `target/` (`Swatinem/rust-cache`) never reached the mutants. In place
  they reuse it: on a small crate with two dependencies the first mutant's build
  went from 4.0 s to 0.2 s. Mutants then run one at a time (`jobs` is ignored).
  Off by default; meant for CI checkouts.
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
