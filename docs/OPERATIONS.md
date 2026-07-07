# Operating the gate in CI

The gate is a CI tool, not a service — there is nothing to deploy or back up.
Operating it means: giving the runner what the gate needs, keeping the
per-mutant compile-and-test cycle inside your CI budget, and reading its
failure modes correctly. Wiring and rollout live in [`ACTION.md`](../ACTION.md)
and [`GUIDE.md`](../GUIDE.md); the knobs themselves are in
[`CONFIG.md`](CONFIG.md); this page is the runbook.

## Runtime requirements

| Requirement | Why | Notes |
| --- | --- | --- |
| **Linux runner** | `cargo-mutants` fails on Windows with a recursive temp-path error (`MAX_PATH`), so mutants can't be tested there. | The Action targets `ubuntu-latest`. The gate detects a part-way engine death via its accounting guard and exits `1` rather than silently passing (src/mutants.rs). macOS works but is slower/pricier. |
| **Rust toolchain + your project's deps** | Each mutant is compiled and tested with *your* toolchain. | `dtolnay/rust-toolchain@stable` in the workflow. |
| **`cargo-mutants` on PATH** | The Rust engine shells out to it. Missing = **fatal** (exit 1 with an install hint). | The Action installs it (`cargo install cargo-mutants --locked`) if absent. |
| **Full git history for the base ref** | The merge-base must be reachable or there is no diff. | `actions/checkout@v4` with `fetch-depth: 0` (the Action also deep-fetches the base ref before `git merge-base`). |
| **`jq` on the runner** | The Action's release-asset install path parses the GitHub API with it. | Present on GitHub-hosted runners. |
| **Optional engines' tools** | `cosmic-ray` (Python), Stryker (JS/TS), gremlins (Go), PIT (JVM). Missing = **warn + skip** that language, never fatal. | `enable-python: "true"` installs Python + cosmic-ray; the others are bring-your-own. |
| **No git binary needed by the gate itself** | The diff is computed in-process with `gix`. | Only the Action's merge-base step uses the `git` CLI. |

Tokens: `GITHUB_TOKEN` with `pull-requests: write` only if you want the PR
comment; a `release-token` only if the action repo is private; `METRICS_TOKEN`
only for a telemetry POST (HTTPS enforced; the Action sets it from its
`metrics-token` input and masks it). None are needed for the verdict itself.

## Cache behaviour & where the time goes

>90% of wall-clock is the engine's per-mutant compile-and-test cycle
(measured in [`BENCHMARK.md`](../BENCHMARK.md); the orchestrator itself is
~11 ms). So operations = managing the build cache:

- **Warm `target/` across runs** (`Swatinem/rust-cache@v2`). The baseline
  build is the single biggest cold-start cost.
- **Keep `CARGO_INCREMENTAL` unset or `1`.** Setting it to `0` (a common CI
  "optimization") forces a full recompile *per mutant*.
- **`--skip-preflight` when CI already ran the suite green this run.** It also
  passes `--baseline skip` to cargo-mutants, skipping the engine's own
  unmutated baseline build+test.
- **`--test-tool nextest`** — per-process, highly parallel test phase, often
  2–3× faster (the Action installs `cargo-nextest` when selected).
- The gate itself writes only a temp work dir (deleted on exit) and, if asked,
  the metrics file. It never mutates your working tree in place —
  cargo-mutants works on copies.

## Budget knobs (bounding a run)

All defined in [`CONFIG.md`](CONFIG.md); ROI-ordered tuning list in
[`GUIDE.md` §5](../GUIDE.md). The short version:

| Knob | Default | Effect on cost |
| --- | --- | --- |
| `max-per-function` | 5 | Directly caps the mutant count — the dominant cost driver. |
| `timeout` | 60 s | Bounds a hung/slow mutant; each timeout burns the full budget. |
| `jobs` | min(cores, 8) | Parallelism; raise on bigger runners. |
| `test-changed-package-only` | off | Narrows the per-mutant test run in a workspace (may surface downstream-only false survivors). |
| `slop-gate estimate` | — | Predict the mutant count + latency band **before** paying for a run. |

## Degraded modes (what the gate does when it can't do its job)

The gate never silently passes; every shed path is visible in the report:

| Situation | Behaviour | Verdict impact |
| --- | --- | --- |
| No mutatable language in the diff | Short-circuits: "nothing to mutate" | **Pass**, exit 0. |
| Suite red or flaky in the pre-flight | That language's mutation is **suppressed** (survivors would be indistinguishable from flakiness); static lanes still run | **Blocks** by default ("suite not green & stable"); advisory mode reports only. |
| `cargo-mutants` missing | Fatal with install hint | Exit **1** (operational). |
| Optional engine tool missing (cosmic-ray, Stryker, …) | Warn + skip that language | No impact from that engine. |
| Engine died mid-run / inconsistent accounting | Fatal — survivor count can't be trusted | Exit **1**, never a silent pass. |
| PR comment or telemetry fails | Warning on stderr; the telemetry POST is also time-bounded (5 s connect/read/write) so a slow sink can't stall the run | None — best-effort by design. |
| `--advisory` | Everything reported, survivor/zero-assertion gates off | Exit 0 unless an explicitly enabled gate (severity/debt/pattern) trips. |

## Troubleshooting (symptom-first)

| You see | Cause | Fix |
| --- | --- | --- |
| Exit 2 — `N surviving mutation(s) exceed the allowed M` | Real signal: the suite passed over mutations on your changed lines. | Add the missing assertions (the survivor table names file:line and the exact mutation), or consciously raise `max-survivors`. |
| Exit 2 — `test suite was not green & stable, so mutation results can't be trusted` | The determinism pre-flight found the suite red or flaky. | Fix the red test; deflake (the pre-flight runs the suite `preflight_runs`=2×). `--skip-preflight` only if CI *already proved* the same tree green — it does not make flaky results trustworthy. |
| Exit 1 — `cargo-mutants not found on PATH` | Engine not installed (the probe runs `cargo mutants --version`). | `cargo install cargo-mutants` (the Action does this automatically). |
| Exit 1 — diff errors / no merge-base | Shallow checkout; the base ref isn't reachable. | `fetch-depth: 0` on checkout; on non-PR events set `base-ref` explicitly. |
| Action step fails — `no base ref (set base-ref, or run on pull_request)` | Ran on `push` without a `base-ref` input (`github.base_ref` is empty outside PRs). | Set the `base-ref` input, or trigger on `pull_request`. |
| Exit 1 — `cargo mutants accounted for N mutant(s), expected M … run likely failed immediately` | The engine died part-way (typically the Windows temp-path bug). | Run on Linux (`ubuntu-latest`, or the [`test-harness/`](../test-harness) container locally on Windows). |
| Warning — `could not post PR comment` | Missing/underscoped token, or not in a PR context. Verdict is unaffected. | Grant `permissions: pull-requests: write` and pass `token`; ensure the run is PR-triggered (or set `PR_NUMBER`). |
| Error — `metrics_url must be an https:// URL with a host` | Config validation: telemetry can carry the `METRICS_TOKEN` bearer, so plaintext endpoints are refused. | Use an `https://` sink. |
| Warning — `could not post metrics` | The `--metrics-url` sink was unreachable, slow (each POST phase times out after 5 s), or rejected the record (e.g. bad/missing `metrics-token` on an authenticated Mergestro `/v1/ingest`). Verdict is unaffected. | Check the URL and token; the run's record is still in `--metrics-file` if you set one. |
| `::notice::release … not found (or unreadable); building from source` + slow cold start | The pinned `version` tag has no release/musl asset visible to your token. | Pin `version` to an existing release tag; for a private action repo supply `release-token`. |
| Runs are slow / over CI budget | Per-mutant compile-and-test dominates. | Work the cache + budget knobs above, in that order; run `slop-gate estimate` in review to predict cost. |
| Mutation silently skipped for `.py` / `.js` / `.go` / Java files | Optional engine tool not installed (warn+skip is by design for PoC engines). | Install the tool (`enable-python` for cosmic-ray) — and treat those engines as advisory PoCs. |
| Timeouts counted (`timed_out > 0`) | Slow suite or a mutant causing an infinite loop (e.g. mutated loop condition). | Raise `timeout` if the *baseline* suite is near the limit; otherwise timeouts are the engine correctly bounding pathological mutants. |

## Security notes

- The gate **executes your project's build and tests** (and, with mutation,
  many variants of them). Treat a PR's code as untrusted exactly as you would
  in any CI job that runs `cargo test` on it — same-repo PRs on
  `pull_request` is the intended, least-privilege setup.
- Tokens: the comment token needs only `pull-requests: write`; the Action
  masks `release-token` with `::add-mask::` and scopes its git-credential
  rewrite to the action repo only; `METRICS_TOKEN` is only ever sent over
  HTTPS (enforced by config validation).
- The gate makes no network calls at all unless you enable `--comment` /
  `--metrics-url` (binary/tool installs are the Action's steps, not the gate).
