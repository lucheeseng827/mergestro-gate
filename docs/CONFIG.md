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
| `preflight_runs` | `--preflight-runs` | — | u32 ≥ 1 | `2` | How many times the suite runs in the determinism pre-flight (all must be green and agree). |
| `skip_preflight` | `--skip-preflight` | — | bool | `false` | Skip the pre-flight **and** pass `--baseline skip` to cargo-mutants. Use when CI already proved the suite green this run. |
| `test_command` | — | — | list of strings | `["cargo", "test", "--quiet"]` | Command the Rust pre-flight runs (program first). |
| `python_test_command` | — | — | list of strings | `["python3", "-m", "pytest", "-q"]` | Pre-flight + cosmic-ray test command for changed `.py` files (advisory PoC). |
| `test_changed_package_only` | `--test-changed-package-only` | `test-changed-package-only` | bool | `false` | In a workspace, narrow mutation *tests* to the changed crate (faster; a mutant caught only by a downstream crate's tests then shows as a survivor). Default runs the whole workspace's tests per mutant. |
| `test_tool` | `--test-tool` | `test-tool` | `cargo` \| `nextest` | `cargo` | Test runner cargo-mutants drives. `nextest` is often 2–3× faster; needs `cargo-nextest` (the Action installs it when selected). |
| `block_on_survivors` | (off via `--advisory`) | (off via `advisory`) | bool | `true` | Whether survivors over budget — or an untrustworthy suite — block. `--advisory` sets this (and `block_on_zero_assertion_tests`) to false. |
| `max_survivors` | `--max-survivors` | `max-survivors` | usize | `0` | Survivors tolerated before blocking. `0` = any survivor blocks. |
| `check_zero_assertion_tests` | — | — | bool | `true` | Run the static zero-assertion test pre-check at all. |
| `block_on_zero_assertion_tests` | `--block-on-zero-assertion` | `block-on-zero-assertion` | bool | `false` | Also block when assertion-free tests are found (heuristic, so advisory by default). |
| `block_on_severity` | `--block-on-severity` | `block-on-severity` | `low`\|`medium`\|`high`\|`critical` | unset (advisory) | Block when any survivor reaches this tier, regardless of count. Severity always orders the report either way. |
| `debt_budget` | `--debt-budget` | `debt-budget` | i64 ≥ 0 | `25` | Per-PR structural-debt budget (net complexity + duplication + coupling). `0` disables the burn-rate framing. |
| `block_on_debt` | `--block-on-debt` | `block-on-debt` | bool | `false` | Block when the debt-delta exceeds the budget (always reported; gates only when set). |
| `block_on_pattern` | `--block-on-pattern` (comma-separated / repeatable) | `block-on-pattern` (comma-separated) | list | empty (advisory) | Turn a pattern lane into a hard gate. Each entry is a lane (`slop`\|`security`\|`convention`\|`docs`\|`all`) or a rule id (lowercase `a-z0-9-`, e.g. `hardcoded-secret`, `unknown-crate-import`, `docs-stale-config`). |
| `metrics_file` | `--metrics-file` | `metrics-file` | path | unset | Append a JSON-Lines validation record per run (schema in [`API.md`](API.md#validation-telemetry--the-jsonl-record)). |
| `metrics_url` | `--metrics-url` | `metrics-url` | URL | unset | POST the run record to a telemetry endpoint — typically a Mergestro `/v1/ingest` URL, so hosted fleet views see the run. Best-effort and time-bounded (5 s connect/read/write, `METRICS_POST_TIMEOUT` in metrics.rs): a slow or unreachable endpoint warns and never fails the gate. **Must be `https://` with a real host** — validation rejects anything else because the request can carry the `METRICS_TOKEN` bearer. |

CLI-only flags on the gate run:

| Flag | Type | Default | What it does |
| --- | --- | --- | --- |
| `--config <path>` | path | unset | Load a YAML config file (all keys above; every field optional). |
| `--advisory` | bool | `false` | Report but never block on survivors / zero-assertion tests (Phase 1 behaviour). Severity/debt/pattern gates you explicitly enabled still apply. |
| `--comment` | bool | `false` (Action input `comment` defaults **`"true"`**) | Post/update the idempotent PR comment. Best-effort — failure warns, never changes the verdict. |
| `--format <text\|json\|markdown>` | enum | `text` | Output format on stdout. |

## Subcommands

**`slop-gate estimate`** — dry-run projection (enumerate + cap, no build):
`--repo`, `--base`, `--head`, `--config`, `--max-per-function` as above, plus
`--json` (machine-readable output; schema in [`API.md`](API.md#estimate---json-output)).

**`slop-gate analyze`** — `--metrics-file <path>` (required): read the JSONL
telemetry back into the KPI summary (text output).

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
| `version` | `v0.5.0` | Release tag of the prebuilt musl binary to install (falls back to a source build if the release/asset is missing). |
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
