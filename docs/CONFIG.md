# Configuration reference — every knob in one place

Source of truth: the clap derives in [`src/main.rs`](../src/main.rs) (`RunArgs`,
`EstimateArgs`, `AnalyzeArgs`), the `Config` struct in
[`src/config.rs`](../src/config.rs), and the composite-Action inputs in
[`action.yml`](../action.yml). Regenerate this table when any of those change —
do not hand-edit it out of sync.

**Precedence:** built-in defaults ← YAML file (`--config` /
[`mergestro-gate.example.yaml`](../mergestro-gate.example.yaml)) ← CLI flags.
Action inputs are just a shell around the CLI flags (see the last step of
`action.yml`), so they sit at the CLI layer.

## The gate's knobs (CLI ↔ YAML ↔ Action)

Every knob of the default gate run. "—" means that surface doesn't expose the
knob.

| YAML key (`Config`) | CLI flag | Action input | Type | Default | What it does / when to change it |
| --- | --- | --- | --- | --- | --- |
| `repo` | `--repo` | — (always `.`) | path | `.` | Repository under test. |
| `base_ref` | `--base` | `base-ref` (resolved to the merge-base first) | string | `origin/main` | Ref the diff is taken against. Pass the **merge-base** for PR semantics; the Action computes it for you (`git merge-base`). |
| `head_ref` | `--head` | — (always `HEAD`) | string | `HEAD` | Ref being gated. |
| `jobs` | `--jobs` | `jobs` | usize ≥ 1 | `min(cores, 8)`, fallback 1 (`default_jobs()`, config.rs) — Action default `"4"` | Parallel mutant jobs (`cargo-mutants --jobs`). Raise on bigger runners. |
| `timeout_secs` | `--timeout` | `timeout` | u64 secs ≥ 1 | `60` | Per-mutant test timeout (`cargo-mutants --timeout`). Raise for slow suites so mutants don't count as timed-out. |
| `max_mutants_per_function` | `--max-per-function` | `max-per-function` | usize ≥ 1 | `5` | Hard cap on mutants tested per function — the core runtime lever. Lower to bound cost, raise for more thorough runs. |
| `budget_secs` | `--budget` (`600`, `90s`, `10m`, `1h`) | `budget` | u64 ≥ 1, optional | none | Wall-clock limit on the Rust mutation run, cargo-mutants' first build included (the pre-flight is not counted). At the limit the run is stopped: finished mutants count as usual, the rest are reported as **not tested**, never as caught or surviving. Mutants run in source order (`--no-shuffle`), so the untested set is the same on a rerun. |
| `block_on_budget` | `--block-on-budget` | `block-on-budget` | bool | `false` | Block when the budget left mutants untested. Otherwise it is a warning in the report. `--advisory` turns it off unless the flag is also given. |
| — | `--sarif <PATH>` | `sarif` (+ `upload-sarif`) | path | none | Also write the findings — survivors (level from severity), zero-assertion tests, and the slop / security / convention / docs lanes — as SARIF 2.1.0 to this path (relative to the working directory, like every output path; missing parent directories are created), independent of `--format`. The file paths *inside* the SARIF are relative to `--repo`, so upload from a run whose `--repo` is the repository root. |
| — | `--comment-inline` (needs `--comment`) | `comment-inline` | bool | `false` | Also post each surviving mutant as a review comment on its line (GitHub only), once each: the summary records which have an inline comment only after the review posts, so a failed review is retried next run. Only lines the diff shows are commented; the rest stay in the summary. Refused by `merge-reports` (no diff). |
| `shard` | `--shard k/n` | — | `k/n`, 1 ≤ k ≤ n | none | Run one shard of the Rust mutants (round-robin over the kept list); combine the shards' `--format json` reports with `slop-gate merge-reports`. See OPERATIONS.md. |
| `in_place` | `--in-place` | `in-place` | bool | `false` | Mutate the checkout (`cargo-mutants --in-place`) instead of a scratch copy. The copy starts without `target/`, so each run pays a cold build even after the pre-flight built the tree; in place, mutants reuse that build and a cached `target/`. Mutants then run one at a time (`jobs` ignored), and files in the checkout are edited while it runs. For CI checkouts. |
| `preflight_runs` | `--preflight-runs` | — | u32 ≥ 1 | `1` | How many times the suite runs in the determinism pre-flight (all must be green and agree). `1` proves it green; `2`+ also catches a flaky suite, at one full suite run each. cargo-mutants' own baseline is always skipped, because mutation only starts after a green pre-flight. |
| `skip_preflight` | `--skip-preflight` | — | bool | `false` | Skip the pre-flight. Use when CI already proved the suite green this run: no suite run happens before the first mutant. |
| `test_command` | — | — | list of strings | `["cargo", "test", "--quiet"]` | Command the Rust pre-flight runs (program first). |
| `python_test_command` | — | — | list of strings | `["python3", "-m", "pytest", "-q"]` | Pre-flight + cosmic-ray test command for changed `.py` files (advisory PoC). |
| `test_changed_package_only` | `--test-workspace` sets `false` | `test-workspace: true` sets `false` | bool | `true` | In a workspace, each mutant runs only the changed crate's tests (the fast path). Set `false` to run the whole workspace's tests per mutant, so a mutant caught only by a downstream crate's tests is still caught. Was `false` before 0.6.0; `--test-changed-package-only` is kept and now changes nothing. |
| `test_tool` | `--test-tool` | `test-tool` | `cargo` \| `nextest` | `cargo` | Test runner cargo-mutants drives. `nextest` is often 2–3× faster; needs `cargo-nextest` (the Action installs it when selected). |
| `block_on_survivors` | (off via `--advisory`) | (off via `advisory`) | bool | `true` | Whether survivors over budget — or an untrustworthy suite — block. `--advisory` sets this (and `block_on_zero_assertion_tests`) to false. |
| `max_survivors` | `--max-survivors` | `max-survivors` | usize | `0` | Survivors tolerated before blocking. `0` = any survivor blocks. |
| `check_zero_assertion_tests` | — | — | bool | `true` | Run the static zero-assertion test pre-check at all. |
| `block_on_zero_assertion_tests` | `--block-on-zero-assertion` | `block-on-zero-assertion` | bool | `false` | Also block when assertion-free tests are found (heuristic, so advisory by default). |
| `block_on_severity` | `--block-on-severity` | `block-on-severity` | `low`\|`medium`\|`high`\|`critical` | unset (advisory) | Block when any survivor reaches this tier, regardless of count. Severity always orders the report either way. |
| `debt_budget` | `--debt-budget` | `debt-budget` | i64 ≥ 0 | `25` | Per-PR structural-debt budget (net complexity + duplication + coupling). `0` disables the burn-rate framing. |
| `block_on_debt` | `--block-on-debt` | `block-on-debt` | bool | `false` | Block when the debt-delta exceeds the budget (always reported; gates only when set). |
| `turnover.enabled` | `--no-turnover` (to disable) | `turnover` | bool | `true` | Run the turnover maintainability-drift lane. It reads history, not the diff, so it runs even when nothing is mutable. |
| `turnover.baseline` | `--turnover-baseline` | `turnover-baseline` | path | `.turnover/baseline.json` | The per-commit baseline file. Build once with `slop-gate baseline` (full history), then commit or cache it; every run refreshes it incrementally in memory (`turnover.update_baseline: true` writes it back). Missing file = lane **skipped** with a hint. |
| `turnover.block_on_drift` | `--block-on-turnover-drift` | `block-on-turnover-drift` | bool | `false` | Turn a drifted verdict into a block reason. Advisory otherwise — the numbers and the PR-comment section always render. |
| `turnover.refresh` / `turnover.update_baseline` | — | — | bool | `true` / `false` | Walk the PR's new commits before judging; persist the refreshed file afterwards. |
| `turnover.baseline_url` | `--turnover-baseline-url` | `turnover-baseline-url` | URL | unset | The Mergestro plane's baseline service. When set, the lane fetches the repo's baseline from the plane before judging and (with `update_baseline`) pushes the refreshed file back; `slop-gate baseline` pushes the file it builds. Authenticated with `METRICS_TOKEN`. Removes the "commit it or cache it" step entirely once one runner has built the file. |
| `turnover.gate` / `thresholds` / `drift` / `signals` / `churn` / `walk` | — | — | maps | turnover's defaults | The turnover policy, key for key the same as `turnover.toml`: `gate.window_days` (90), `gate.min_added_lines` (200 — below it the lane reports *insufficient sample* and passes), `drift.*_max_rise` / `drift.refactor_max_drop` (0.05 each), optional absolute `thresholds`, classifier `signals`, `churn.horizon_days` (14), `walk.ignore_paths`. |
| `block_on_pattern` | `--block-on-pattern` (comma-separated / repeatable) | `block-on-pattern` (comma-separated) | list | empty (advisory) | Turn a pattern lane into a hard gate. Each entry is a lane (`slop`\|`security`\|`convention`\|`docs`\|`weakened-tests`\|`all`) or a rule id (lowercase `a-z0-9-`, e.g. `hardcoded-secret`, `unknown-crate-import`, `docs-stale-config`, `test-removed`, `assertions-reduced`). |
| `mcp_servers` | — (YAML only) | — | list of server entries | empty (lane off) | First-party MCP servers this repo ships. Declaring one opts the repo in; a PR touching its `paths` is probed before it can merge. Entry fields below. |
| `mcp_fail_on` | `--mcp-fail-on` | — | `never`\|`critical`\|`unproven`\|`any` | `critical` | How strict the MCP lane is. `critical` blocks on the tier that would deny the server admission; `unproven` also blocks when a Critical check could not be run; `any` blocks on any failure. **A skipped check never blocks at any setting.** `--advisory` sets this to `never`; an explicit `--mcp-fail-on` is narrower and wins. |
| `specprobe_bin` | `--specprobe-bin` | — | path/name | `specprobe` | The prober binary the MCP lane drives. A bare name is resolved on `PATH`. |
| `metrics_file` | `--metrics-file` | `metrics-file` | path | unset | Append a JSON-Lines validation record per run (schema in [`API.md`](API.md#validation-telemetry--the-jsonl-record)). |
| `metrics_url` | `--metrics-url` | `metrics-url` | URL | unset | POST the run record to a telemetry endpoint — typically a Mergestro `/v1/ingest` URL, so hosted fleet views see the run. Best-effort and time-bounded (5 s connect/read/write, `METRICS_POST_TIMEOUT` in metrics.rs): a slow or unreachable endpoint warns and never fails the gate. **Must be `https://` with a real host** — validation rejects anything else because the request can carry the `METRICS_TOKEN` bearer. |

Each `mcp_servers` entry:

| Field | Type | Default | What it does |
| --- | --- | --- | --- |
| `name` | string (required, unique) | — | Label in the report and the PR comment. |
| `paths` | list of strings (required, non-empty) | — | Repo-relative path prefixes; a diff touching one puts this server in scope. Matching is per path *segment*, so `servers/acme` does not claim `servers/acme-unrelated`. Scoping reads every changed path — deletions and any extension — not the unified diff, so a changed tool schema or Dockerfile counts. |
| `command` | list of strings (required, non-empty) | — | The server to spawn, program first. Arguments go to the prober as JSON, never a shell string. |
| `build` | list of strings | empty | Command run before probing, program first. A failing build **blocks** — the gate cannot certify a server it could not produce. |
| `dir` | string | repo root | Working directory for the build and probe. Must be repo-relative with no `..`: this file is editable by a pull request, so the lane stays inside the checkout. |
| `spec_version` | string | `2025-11-25` | Revision to probe against. `2026-07-28` or later probes the stateless lane; checks outside the era skip rather than failing. |
| `timeout_secs` | u64 ≥ 1 | `20` | Per-exchange deadline. A hang is a failure, so this bounds the whole probe. |
| `elicit_tool` | string | unset | A tool on this server whose call raises an elicitation. Without it the elicitation check skips — the name cannot be discovered from the wire, and guessing would score an `unknown tool` error as a pass. |

CLI-only flags on the gate run:

| Flag | Type | Default | What it does |
| --- | --- | --- | --- |
| `--config <path>` | path | unset | Load a YAML config file (all keys above; every field optional). |
| `--advisory` | bool | `false` | Report but never block on survivors / zero-assertion tests (Phase 1 behaviour). Severity/debt/pattern gates you explicitly enabled still apply. |
| `--comment` | bool | `false` (Action input `comment` defaults **`"true"`**) | Post/update the idempotent PR comment. Best-effort — failure warns, never changes the verdict. |
| `--format <text\|json\|markdown>` | enum | `text` | Output format on stdout. |

## Subcommands

- `slop-gate baseline [--full] [--since-days N]` — build or refresh the turnover baseline for
  the repo (`--repo`, `--head`, `--config`, `--turnover-baseline` apply). Run it once with full
  history; the gate refreshes it incrementally from then on. `--full` discards the file and
  re-walks (required after changing `turnover.signals`).

**`slop-gate estimate`** — dry-run projection (enumerate + cap, no build):
`--repo`, `--base`, `--head`, `--config`, `--max-per-function` as above, plus
`--json` (machine-readable output; schema in [`API.md`](API.md#estimate---json-output)).

**`slop-gate analyze`** — `--metrics-file <path>` (required): read the JSONL
telemetry back into the KPI summary (text output).

**`slop-gate progression`** — resolve the repository's authored milestone tree
against its own commits and PRs, and render it. Full reference, including the
spec format, in [`PROGRESSION.md`](PROGRESSION.md).

| Flag | Meaning |
| ---- | ------- |
| `--spec <path>` | The plan, YAML (required — `progression init` writes one). See [`mergestro-progression.example.yaml`](../mergestro-progression.example.yaml). |
| `--repo <path>` | Repository to resolve against (default `.`). |
| `--head <ref>` | Ref whose ancestry is the history (default `HEAD`). |
| `--since-days <n>` | Ignore older commits; overrides the spec's `since_days`. |
| `--svg <path>` | Write the drawing. Deterministic — meant to be committed. |
| `--readme <path>` | Rewrite the block between the `mergestro:progression` markers. |
| `--markdown <path>` | Write that block standalone. |
| `--json <path>` | Write the snapshot. Carries the resolve time, so **not** a committed artifact. |
| `--svg-href <src>` | `src` for the README's `<img>` (default: `--svg` relative to the README). |
| `--check` | Write nothing; **exit 2** when a requested artifact is stale. |
| `--max-commits <n>` | Stop the walk after this many commits (default 20000). The walk warns when it hits the cap — everything past it is invisible and milestones are understated. |
| `--allow-shallow` | Resolve against a truncated history anyway (it is refused by default). |
| `--plane-url <url>` | POST the snapshot to a Mergestro ingest endpoint. Token: `METRICS_TOKEN`. Skipped under `--check` — a dry run leaves no record behind. |
| `--slug <owner/repo>` | Repo the snapshot is filed under (default `GITHUB_REPOSITORY`). |
| `--format text\|json\|markdown` | What the command prints (default `text`). |

**`slop-gate progression init`** — scaffold a first plan from the repository's
own history, for a repository that does not have one yet. Mines the components
that carry work and the marker the merges write (the two things a hand-written
first spec gets wrong silently), and leaves one milestone open for the work you
are planning. Takes none of the flags above.

| Flag | Meaning |
| ---- | ------- |
| `--repo <path>` | Repository to mine (default `.`). |
| `--out <path>` | Where to write (default `progression.yaml`). Refuses to overwrite without `--force`. |
| `--head <ref>` | Ref whose ancestry to mine (default `HEAD`). |
| `--title <text>` | Plan heading (default: the repository's directory name). |
| `--season <text>` | Period label — "2026 H1", "Sprint 14". |
| `--since-days <n>` | Mine only the last N days, and write that window into the draft. Makes the tree a sliding window: a milestone closed by commits that later fall out of it re-opens. |
| `--max-nodes <n>` | Cap on mined milestones (default 8). |
| `--min-commits <n>` | Commits a directory needs to become a milestone (default 3). |
| `--depth <n>` | How many segments deep a component may sit (default 3). |
| `--max-commits <n>` | Stop the walk after this many commits (default 20000). |
| `--stdout` | Print the draft instead of writing it. |
| `--force` | Overwrite an existing file. |

Under two mined components it writes a starter plan (Foundations → core →
hardening → ship, wired to conventional locations) instead — on a young
repository that resolves near zero and fills in as the work lands, which is the
shape a plan retro-fitted to a mature repository never gets back. A shallow
clone is reported rather than refused here (a draft is a draft), but it drops
the phase grouping: a checkout that sees only last week cannot say which
component came first. Full reference in [`PROGRESSION.md`](PROGRESSION.md);
worked plans, a runnable walkthrough and prompts for authoring one with an
assistant in [`examples/progression/`](../examples/progression/).

A shallow checkout is **refused**: `actions/checkout` defaults to depth 1, and a
plan resolved against the last commit reads as barely started — wrong, and
indistinguishable from real regression. Use `fetch-depth: 0`.

## Action-only inputs

These exist only in [`action.yml`](../action.yml) (install/wiring concerns, not
gate knobs). All inputs are optional.

| Input | Default | What it does |
| --- | --- | --- |
| `base-ref` | `""` | Base branch to diff against; defaults to the PR's base branch (`github.base_ref`). The action fetches it and passes `git merge-base` to `--base`. |
| `comment` | `"true"` | Post/update the PR comment (note: the bare CLI defaults this **off**). |
| `enable-python` | `"false"` | Install Python + cosmic-ray so changed `.py` files are mutation-tested (advisory PoC). Your project's own test deps (e.g. pytest) must already be installed. |
| `python-version` | `"3.x"` | Python toolchain version (when `enable-python`). |
| `cosmic-ray-version` | `"8.4.6"` | Pinned cosmic-ray release (when `enable-python`). |
| `version` | `v0.6.0` | Release tag of the prebuilt musl binary to install (falls back to a source build if the release/asset is missing). |
| `metrics-token` | `""` | Bearer token for the `metrics-url` POST. The Action exports it as `METRICS_TOKEN` for the gate's emitter and masks it in logs (`::add-mask::`). Required when `metrics-url` points at an authenticated Mergestro instance. |
| `token` | `""` | GitHub token used to post the PR comment (falls back to `github.token`). |
| `release-token` | `""` | Token with read access to the action repo's releases — required when the action repo is **private** (a consumer repo's default `GITHUB_TOKEN` can't read it). Also authenticates the source-build fallback. Masked in logs. |

## Environment variables

Read at runtime (sources: `src/github.rs`, `src/metrics.rs`, `src/main.rs`).
All are optional unless noted; inside GitHub Actions they are pre-set.

| Variable | Used by | Purpose |
| --- | --- | --- |
| `GITHUB_TOKEN` (fallback `INPUT_TOKEN`) | `--comment` | Auth for the PR comment. **Required** for `--comment`; needs `pull-requests: write`. |
| `GITHUB_REPOSITORY` | `--comment`, telemetry | `owner/repo` — comment target and run-record identity. Required for `--comment`. |
| `GITHUB_API_URL` | `--comment` | API base; defaults to `https://api.github.com` (GHES override). |
| `PR_NUMBER`, `GITHUB_REF`, `GITHUB_EVENT_PATH` | `--comment`, telemetry | PR-number detection, tried in that order (`refs/pull/<n>/…`, then the event payload). |
| `PR_HEAD_SHA` (fallback `GITHUB_SHA`) | telemetry | True PR head for per-PR survivor tracking (the Action sets it; `GITHUB_SHA` is the merge commit). |
| `GITHUB_RUN_ID`, `GITHUB_ACTOR` | telemetry | Run identity in the JSONL record. |
| `METRICS_TOKEN` | `--metrics-url` | Bearer token for the telemetry POST (HTTPS enforced by config validation). The Action sets it from its `metrics-token` input and masks it in logs. |

Cache-relevant (read by the toolchain, not the gate): keep `CARGO_INCREMENTAL`
unset or `1` in CI — `0` forces a full recompile per mutant. See
[`OPERATIONS.md`](OPERATIONS.md#cache-behaviour--where-the-time-goes).

## Validation rules

`Config::validate()` (src/config.rs) rejects nonsense before any subprocess
runs: `jobs`, `timeout_secs`, `max_mutants_per_function`, `preflight_runs`
must be ≥ 1; `test_command` / `python_test_command` non-empty;
`debt_budget` ≥ 0; `test_tool` ∈ {`cargo`, `nextest`}; `metrics_url` must be
`https://` with a host; each `block_on_pattern` entry must be a known lane or a
rule-id-shaped token (catches typos that would otherwise silently never gate).

For the MCP lane: `mcp_fail_on` must be one of the four thresholds and
`specprobe_bin` non-empty; every `mcp_servers` entry needs a unique `name`, at
least one `paths` entry and a non-empty `command`, a `timeout_secs` ≥ 1, a
non-empty `spec_version`, and a `dir`/`paths` that stay repo-relative with no
`..`. A server with no `paths` is rejected rather than accepted-and-never-run:
it would sit in the config looking like coverage.
