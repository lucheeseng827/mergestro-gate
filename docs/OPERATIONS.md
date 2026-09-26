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

- **Warm `target/` across runs** (`Swatinem/rust-cache@v2`) **and set
  `in-place: true`.** The cold build is the single biggest start-up cost, and a
  cached `target/` alone only reaches the pre-flight: cargo-mutants builds
  mutants in a scratch copy with no `target/`. In place, they reuse it; mutants
  then run one at a time.
- **Keep `CARGO_INCREMENTAL` unset or `1`.** Setting it to `0` (a common CI
  "optimization") forces a full recompile *per mutant*.
- **`--skip-preflight` when CI already ran the suite green this run.** Then no
  suite run happens before the first mutant. (cargo-mutants' own baseline is
  always skipped: the gate only mutates after a green pre-flight.)
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
| `test-workspace` | off | Widens the per-mutant test run from the changed crate to the whole workspace. Turn it on when a crate is tested mainly from another crate: with it off, a downstream-only catch shows as a survivor. |
| `slop-gate estimate` | — | Predict the mutant count + latency band **before** paying for a run. |

## Sharding a large run over a CI matrix

`--shard k/n` runs one shard of the Rust mutants; the kept mutants are dealt
round-robin in listing order, so every shard computes the same split. The other
engines run on shard 1 only. Each shard writes a `--format json` report, and one
job merges them: `merge-reports` sums the outcomes, recomputes the verdict under
its own flags (so `--max-survivors` applies to the total) and comments once. It
refuses a missing, duplicated or foreign shard, since a missing shard is
untested mutants rather than a clean result.

```yaml
jobs:
  shard:
    strategy: { matrix: { shard: [1, 2, 3, 4] } }
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with: { fetch-depth: 0 }
      - uses: dtolnay/rust-toolchain@stable
      # install cargo-mutants and slop-gate as in ACTION.md, then:
      - run: slop-gate --base origin/main --shard ${{ matrix.shard }}/4 --format json --advisory > shard.json
      - uses: actions/upload-artifact@v4
        with: { name: "shard-${{ matrix.shard }}", path: shard.json }
  merge:
    needs: shard
    runs-on: ubuntu-latest
    permissions: { contents: read, pull-requests: write }
    steps:
      - uses: actions/download-artifact@v4
      - run: slop-gate merge-reports --base origin/main --comment */shard.json
```

Run the shards `--advisory` (a shard's own verdict means nothing) and let the
merge job decide: it exits 2 when the combined run blocks.

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
| MCP lane: `specprobe` missing, the server's build fails, or the prober's output is unreadable | Reported as `did not run`, with the reason | **Blocks.** The gate cannot certify what it did not run — see the note below. |
| MCP lane: checks the transport or the era cannot express | Reported as skips, **with their count**, in every render | None — skips are weightless at every threshold. |
| Turnover lane: no baseline for this repo yet | Reported as `not measured`, naming the remedy **for that wiring** — no plane configured, a plane holding none for this repo yet, and an unreachable plane are three different fixes and get three different hints | None — **skipped**. The gate cannot certify a trajectory it has not measured. |
| Turnover lane: plane unreachable, or `TURNOVER_TOKEN` absent (every fork pull request) | Warning on stderr; the lane falls back to whatever baseline is on disk, usually none, and the `not measured` reason says the fetch failed rather than implying nothing was configured | None — best-effort, same posture as telemetry. |
| Turnover lane: `[signals]` changed since the baseline was built | Reported as skipped, naming `--full` | None — old rows are incomparable, and mixing them would report a number built from two definitions. |
| `--advisory` | Everything reported, survivor/zero-assertion gates off, MCP lane advisory | Exit 0 unless an explicitly enabled gate (severity/debt/pattern) trips. |

The MCP lane is the one degraded mode that *blocks* rather than skipping, and it
is deliberate: the other lanes read the diff, so a missing tool costs a signal;
this one runs the artifact, so a missing tool means nobody checked whether the
server still works. Reporting that as a pass would be the gate taking credit for
a check it never performed. If you want it advisory while you roll it out, that
is what `mcp_fail_on: never` (or `--advisory`) is for.

## The turnover baseline, via the plane (this repository)

The maintainability lane needs one classified row per commit before it can judge
drift. This repository keeps that file in the Mergestro plane's baseline service
rather than in git or an Actions cache, so no runner needs a full clone or a cache
key that survived.

**One writer.** `turnover-baseline-seed.yml` builds and refreshes the baseline —
daily, and on demand. The per-PR gate `slop-gate-selfgate.yml` only *fetches* it:
`turnover.update_baseline` stays at its default `false`. That is not a tuning
choice. The plane's `PUT` is a blind upsert with no version check, so concurrent
PR runs would each overwrite the other's rows; `run_gate` records the checkout's
HEAD as the point every later walk starts from, which on `pull_request` is the
synthetic merge commit and stops existing when the PR closes; and the walk would
fold the PR's own branch commits into the shared baseline, which on a
squash-merging repository means rows for shas that never reach main. A PR run
that writes corrupts the measurement three different ways, so it does not write.

Two repository settings switch it on — the URL is an address rather than a policy,
so it is deliberately not in the PR-editable config file:

| Setting | Kind | Value |
| --- | --- | --- |
| `MERGESTRO_PLANE_URL` | repository **variable** | The plane's `https://` base URL, with **no path** — the client appends `/v1/turnover/baseline/<repo>` itself, so the existing `MERGESTRO_URL` secret is not a substitute: that one ends in `/v1/ingest`. Validation rejects plaintext, because the request carries the bearer. |
| `TURNOVER_TOKEN` | repository **secret** | The plane's ingest token. `token_from_env` reads this name first, so it needs no mapping. Deliberately *not* `MERGESTRO_TOKEN` or `METRICS_TOKEN`: `MERGESTRO_TOKEN` is a GitHub fine-grained PAT that `sync-mergestro-gate.yml` passes as a checkout token for the OSS mirror, and `docs/ci-mergestro-wiring.md` maps that same secret onto `METRICS_TOKEN` for telemetry — reusing either would hand a GitHub write credential to the plane as a Bearer token. |

Then run the **`turnover baseline (seed)`** workflow once to create the file; after
that it refreshes itself daily. It checks out with full history, walks every commit,
and pushes. Leave `since_days` empty: on this repository a whole-history walk is
3317 commits, ~63 s and a 3.3 MiB baseline — 5% of the plane's 64 MiB per-file cap —
so trimming the walk buys nothing and leaves a thinner baseline for the 90-day window
to be judged against.

A stale baseline costs walk time, never correctness: every gate run walks from the
stored head to its own HEAD in memory before judging. The daily refresh exists to
keep that delta small, not to keep the verdict honest.

The daily run's behaviour keys on **`MERGESTRO_PLANE_URL` alone**:

| trigger | `MERGESTRO_PLANE_URL` | `TURNOVER_TOKEN` | result |
| --- | --- | --- | --- |
| `schedule` | unset | either | **skipped** — no build, no red run |
| `schedule` | set | set | runs |
| `schedule` | set | unset | **fails** — half-configured |
| `workflow_dispatch` | any | any | runs; fails loudly if it cannot push |

So a token without a URL still skips: the URL is what says "a plane exists". A job
that goes red nightly until someone sets a variable teaches people to stop reading
the Actions tab, and nobody asked for the 03:17 run. A manual dispatch is the
opposite — someone asked for a seed and needs to be told why it could not happen.
And a URL *with* no token fails deliberately: unconfigured is nothing to do,
half-configured is a mistake to surface.

Seed from CI, not from a laptop. The plane keys each baseline by `GITHUB_REPOSITORY`,
falling back to the checkout's directory name when it is unset, so a local run files
the baseline under `mergestro-gate` while the gate looks for
`lucheeseng827/mergestro-gate` and goes on reporting `not measured`.

Re-run the seed workflow with **`full`** checked after changing anything under
`[signals]` in the turnover policy — the classifier defines what a row means, so
existing rows stop being comparable and the gate refuses to mix them.

One thing to expect on the first green run: the lane classifies each commit as
AI-coauthored or human from the author, the `Co-authored-by` trailers and the
message, and the defaults match this repository's own commit trailers. Measured
over all 3317 commits, 68% classify as non-human (79% over the last 90 days). That
is the tool working as designed — it is a code-turnover measure, and the split is
the point — but it will appear in every PR comment, so it is worth knowing before
it shows up rather than after.

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
| Exit 2 — ``MCP server `x` failed N conformance check(s): NP-…`` | Real signal: the server answered a malformed frame, truncated body or out-of-order request wrongly. The comment names each check and what the probe saw. | Fix the server. To confirm locally, run the prober directly against the same command; to see the failure without blocking while you work, set `mcp_fail_on: never`. |
| Exit 2 — ``the MCP lane could not run for `x`: could not run `specprobe``` | The prober is not on `PATH`. | Install it, or point `specprobe_bin` / `--specprobe-bin` at it. |
| Exit 2 — ``the MCP lane could not run for `x`: the build command … failed`` | The PR broke the server's build. | Fix the build. The probe is not run when the build fails — there is nothing to probe. |
| Exit 2 — `… exited N without a readable gate report` | The prober ran but printed something the lane could not parse — most often a `specprobe` too old to understand `SPECPROBE_FAIL_ON`. | Upgrade the prober. The lane blocks rather than guessing, because reading an absent gate report as "no blocking checks" would silently disable the gate. |
| MCP run says `clear (2 failure(s) below the threshold)` | Real failures at a severity below `mcp_fail_on`. Not a bug — the row states them so "clear" is not mistaken for "found nothing". | Tighten with `mcp_fail_on: any` if you want them to gate. |
| MCP coverage line shows a large `skipped` count | Expected over stdio: the header and cross-principal checks need an HTTP layer, and era-specific checks do not apply to the other era. | Nothing to do. The count is printed precisely so a 16-skip run is not mistaken for a full sweep. |
| Mutation silently skipped for `.py` / `.js` / `.go` / Java files | Optional engine tool not installed (warn+skip is by design for PoC engines). | Install the tool (`enable-python` for cosmic-ray) — and treat those engines as advisory PoCs. |
| Timeouts counted (`timed_out > 0`) | Slow suite or a mutant causing an infinite loop (e.g. mutated loop condition). | Raise `timeout` if the *baseline* suite is near the limit; otherwise timeouts are the engine correctly bounding pathological mutants. |

## Security notes

- The gate **executes your project's build and tests** (and, with mutation,
  many variants of them). Treat a PR's code as untrusted exactly as you would
  in any CI job that runs `cargo test` on it — same-repo PRs on
  `pull_request` is the intended, least-privilege setup.
- The **MCP lane runs your server**, and its `command` and `build` come from
  `mergestro-gate.yaml` — a file a pull request can edit. Anything those
  commands can do, a PR can make them do, **including reading every secret the
  job's environment carries** (`GITHUB_TOKEN`, `METRICS_TOKEN`,
  `release-token`) and sending it anywhere the runner can reach. The
  repo-relative guard on `dir`/`paths` keeps the *working directory* inside
  the checkout; it is not an execution sandbox and does not mitigate this.
  The trust boundary is the same one `test_command` already sits behind, and
  it has to be drawn in the workflow, not the config: run the lane only where
  the code executing is trusted (same-repo `pull_request` from write-access
  authors, never `pull_request_target` with secrets), and give the job the
  minimum — the lane itself needs **no** secrets, so an untrusted-PR workflow
  that runs it should carry none beyond the default read-only token, with the
  comment/telemetry tokens confined to a separate trusted workflow if used.
- Tokens: the comment token needs only `pull-requests: write`; the Action
  masks `release-token` with `::add-mask::` and scopes its git-credential
  rewrite to the action repo only; `METRICS_TOKEN` is only ever sent over
  HTTPS (enforced by config validation).
- The gate makes no network calls at all unless you enable `--comment` /
  `--metrics-url` (binary/tool installs are the Action's steps, not the gate).
