# API reference — the gate's machine contract

`slop-gate` is a CLI, so its "API" is: the **exit code**, the **report JSON**
(`--format json`), the **telemetry JSONL record** (`--metrics-file` /
`--metrics-url`), the **estimate JSON** (`estimate --json`), and the
**PR comment** it posts. All schemas below are generated from the serde structs
they cite — regenerate this file when those structs change.

Sources: [`src/main.rs`](../src/main.rs) (exit codes),
[`src/report.rs`](../src/report.rs) (`GateReport`, `Verdict`,
`PreflightOutcome`, `Mutant`, `COMMENT_MARKER`),
[`src/verdict.rs`](../src/verdict.rs) (block conditions),
[`src/metrics.rs`](../src/metrics.rs) (`RunMetrics`),
[`src/estimate.rs`](../src/estimate.rs), [`src/github.rs`](../src/github.rs).

## Exit-code contract

Decided in `main()` (src/main.rs):

| Code | Meaning | Exact condition |
| --- | --- | --- |
| `0` | **Pass** | The gate ran and the verdict is `pass` (includes `--advisory` runs and "nothing to mutate"). Also: `analyze` / `estimate` succeeded. |
| `2` | **Blocked** | The gate ran and the verdict is `block` (`EXIT_BLOCKED`, src/main.rs). This is the code a required status check should fail on. A blocked verdict is a *normal, reported outcome* — the report is still printed and the comment/telemetry still emitted. |
| `1` | **Operational failure** | Any error before a verdict could be produced: can't diff (no merge-base, not a repo), `cargo-mutants` missing, config invalid, the mutant-accounting guard tripped (engine died mid-run — see [OPERATIONS.md](OPERATIONS.md#troubleshooting-symptom-first)), etc. Printed as `slop-gate: <error>` on stderr. |

### What blocks (verdict engine, `src/verdict.rs::decide`)

Reasons accumulate; the verdict is `block` if any gate trips:

1. `block_on_survivors` (default on; off in `--advisory`):
   - Rust files changed **and** the pre-flight was not green & stable →
     `"test suite was not green & stable, so mutation results can't be trusted"`.
   - Otherwise, `survivors > max_survivors` (default 0) → blocked.
2. `block_on_zero_assertion_tests` (opt-in): any assertion-free test found.
3. `block_on_severity` (opt-in): worst survivor tier ≥ the threshold, regardless of count.
4. `block_on_debt` (opt-in): debt-delta score > `debt_budget`.
5. `block_on_pattern` (opt-in): a configured lane (`slop`/`security`/`convention`/`docs`/`all`)
   has findings, or a configured rule id matched in any lane.
6. The **MCP lane** (on by default once `mcp_servers` declares a server; `--advisory` and
   `mcp_fail_on: never` turn it off). Two ways a server blocks, and only two:
   - the prober returned checks the configured threshold gates on →
     ``"MCP server `x` failed N conformance check(s): NP-…"``;
   - the lane could not produce a result at all — no prober, a failing build, output it
     cannot read → ``"the MCP lane could not run for `x`: …"``. A lane that could not run is
     not a lane that passed.

   A **skipped** check never blocks, at any threshold. Over stdio roughly a third of the
   catalog cannot be expressed, and era-specific checks do not apply to the other era.

The PR comment and telemetry are **best-effort side effects**: a token or
network failure prints a warning and never changes the verdict or exit code.

## Report JSON (`--format json`)

`GateReport` serialized with serde (pretty-printed). Fields
(src/report.rs, `GateReport`):

| Field | Type | Notes |
| --- | --- | --- |
| `base_ref`, `head_ref` | string | The diffed range. |
| `changed_rust_files` | string[] | Paths, repo-relative. |
| `changed_python_files`, `changed_js_files`, `changed_go_files`, `changed_jvm_files` | string[] | Empty unless that adapter scanned anything. |
| `preflight` | object | Tagged enum, `{"status": "skipped"}` \| `{"status":"passed","runs":N}` \| `{"status":"failed","run":N,"detail":"…"}` \| `{"status":"unstable","detail":"…"}`. |
| `candidates` | int | Mutants enumerated on the changed lines, pre-cap. |
| `capped_out` | int | Dropped by the per-function cap (not tested). |
| `tested`, `caught`, `timed_out`, `unviable` | int | Per-mutant outcomes. |
| `survivors` | Mutant[] | The signal — mutations the suite passed over. |
| `zero_assertion_tests` | object[] | `{file, line, function}` per finding. |
| `debt` | object? | `{complexity, duplication, coupling, added_lines, removed_lines}` (net i64s); omitted when nothing changed. |
| `slop`, `security`, `convention`, `docs` | object? | Pattern-lane reports: `{findings: [{rule, file, line, message, weight}], score}` (score 0–100); omitted when the lane found nothing (`slop` is present, possibly empty, whenever Rust changed). The `docs` lane's findings are module-level (`line` is 0); its rules: `docs-missing-readme`, `docs-missing-architecture`, `docs-missing-event-flow`, `docs-missing-quickstart`, `docs-stale-config`, `docs-stale-api`. |
| `mcp` | object? | MCP lane result; omitted when the lane did not apply (no server declared, or none touched). `{fail_on, servers: [{name, status, …}]}`. Each server is either `{"status":"probed", tally: {scored, passed, failed, errored, skipped}, blocking: [{check, severity?, outcome, reason, detail?}], liveness?, suite_version?}`. `liveness` (present only when the prober reported it — absent means nobody measured, not that the server stayed up) is `{exchanges, timed_out, crashed, wedges, recovered, restarts, ended_down, worst_latency_ms}`; `wedges` is the fault count and `timed_out` is not — an unparseable frame carries no id to answer, so a conformant server drops it or `{"status":"unrunnable", reason}`. An empty `blocking` means clear; `unrunnable` always blocks. Note it is never omitted to mean "ran and found nothing" — a clean run is present with an empty `blocking`. |
| `verdict` | object | `{"decision":"pass"}` \| `{"decision":"block","reasons":["…"]}`. |
| `duration_secs` | float | Wall clock for the whole gate run. |

`Mutant`: `{file, line, column, function?, description, name}` — `name` is the
canonical `cargo-mutants` identity string (`file:line:col: <description>`);
`function` is present only when enumerated via `--list --json`.

Example (blocked run, trimmed):

```json
{
  "base_ref": "abc1234",
  "head_ref": "HEAD",
  "changed_rust_files": ["src/lib.rs"],
  "changed_python_files": [],
  "preflight": { "status": "passed", "runs": 2 },
  "candidates": 13, "capped_out": 0, "tested": 13, "caught": 12,
  "survivors": [
    {
      "file": "src/lib.rs", "line": 42, "column": 12,
      "description": "replace >= with >",
      "name": "src/lib.rs:42:12: replace >= with >"
    }
  ],
  "timed_out": 0, "unviable": 0,
  "zero_assertion_tests": [],
  "debt": { "complexity": 2, "duplication": 0, "coupling": 1,
            "added_lines": 14, "removed_lines": 3 },
  "verdict": { "decision": "block",
               "reasons": ["1 surviving mutation(s) exceed the allowed 0"] },
  "duration_secs": 48.2
}
```

## Validation telemetry — the JSONL record

One `RunMetrics` object per line (src/metrics.rs), appended by
`--metrics-file` and/or POSTed by `--metrics-url` (HTTPS only; optional
`Authorization: Bearer $METRICS_TOKEN`; the POST is time-bounded — 5 s
connect/read/write, `METRICS_POST_TIMEOUT`, so a slow sink can never hang CI).
`slop-gate analyze --metrics-file …` reads these back into the KPI summary
(text output — the summary itself is not serialized).

**Ingest envelope:** the record carries `record_type: "slop"` so the *same*
line drops directly into Mergestro's `/v1/ingest` envelope
(`IngestRecord::Slop`, internally tagged on `record_type` — the queue's
records carry `"queue"`). The ingest side ignores the gate-only fields it
doesn't know; a POST without the tag is rejected there with
"missing field `record_type`".

**Compatibility:** `schema_version` is currently **6**. Every added field
defaults on read, so older records still parse; ingest by ignoring unknown
fields and defaulting missing ones. In particular, pre-v6 local records have
no `record_type` — it defaults to `"slop"` when `analyze` reads them back.
History (doc comment, src/metrics.rs): v2 added
`mutation_score`/`severity_counts`/`debt_score`; v3 `slop_*`; v4
`security_*`; v5 `convention_*`; v6 `record_type`.

| Field | Type | Notes |
| --- | --- | --- |
| `record_type` | string | Always `"slop"` — the ingest discriminator (defaults on read for pre-v6 records). |
| `schema_version` | int | 6. |
| `gate_version` | string | Crate version at build time. |
| `timestamp_unix` | int | Seconds since epoch. |
| `repo`, `head_sha`, `run_id`, `actor` | string? | From the GitHub Actions env; omitted locally. `head_sha` prefers `PR_HEAD_SHA` over `GITHUB_SHA`. |
| `pr` | int? | PR number (`PR_NUMBER` → `GITHUB_REF` → event payload). |
| `mode` | string | `"blocking"` or `"advisory"` (from `block_on_survivors`). |
| `verdict` | string | `"pass"` or `"block"`. |
| `block_reasons` | string[] | Human-readable gate reasons. |
| `changed_rust_files`, `candidates`, `capped_out`, `tested`, `caught`, `survivors`, `timed_out`, `unviable`, `zero_assertion_tests` | int | Counts. |
| `survivor_fingerprints` | string[] | Line-independent hash of file + mutation, so a survivor can be traced across an evolving PR (fix-vs-override). |
| `mutation_score` | float? | `caught / tested`; absent when nothing was tested. |
| `severity_counts` | object | `{critical, high, medium, low}` survivor histogram. |
| `debt_score` | int? | Net structural debt; absent when nothing changed. |
| `slop_score`, `security_score`, `convention_score` | int? | 0–100 lane scores; absent when the lane found nothing. |
| `slop_findings`, `security_findings`, `convention_findings` | int | Lane finding counts. |
| `duration_secs` | float | Run wall clock. |

## `estimate --json` output

Hand-built JSON on stdout (src/estimate.rs); the "changed Python files not
estimated" note goes to **stderr** so stdout stays machine-readable:

```json
{
  "total_candidates": 13,
  "total_tested": 13,
  "total_capped": 0,
  "cap_per_function": 5,
  "est_seconds_warm": 0.39,
  "est_seconds_cold": 6.50,
  "functions": [
    { "group": "arith (src/lib.rs)", "file": "src/lib.rs", "line": 2,
      "candidates": 5, "tested": 5, "capped_out": 0 }
  ]
}
```

The latency band uses fixed per-mutant constants (warm 0.03 s, cold 0.5 s —
src/estimate.rs); it is a projection, not a measurement.

## PR comment

Posted by `--comment` via the GitHub REST API (`ureq`, synchronous —
src/github.rs):

- **Idempotency:** the comment body starts with the hidden marker
  `<!-- slop-gate:report -->` (`COMMENT_MARKER`, src/report.rs). The gate
  lists the PR's comments (`GET /repos/{owner}/{repo}/issues/{pr}/comments`),
  and PATCHes the first comment carrying the marker instead of posting a new
  one — re-runs and merge-queue re-triggers update in place, never stack.
- **Auth/context:** `GITHUB_TOKEN` (or `INPUT_TOKEN`), `GITHUB_REPOSITORY`,
  PR number from `PR_NUMBER` → `GITHUB_REF` (`refs/pull/<n>/…`) →
  `GITHUB_EVENT_PATH` payload; API base from `GITHUB_API_URL` (GHES).
- **Structure** (`GateReport::render_markdown`): verdict badge
  (`✅ Mergestro Gate: passed` / `❌ Mergestro Gate: blocked`), the range +
  changed-file counts + duration, a "Why this is blocked" reason list when
  blocked, then sections that appear only when non-empty: **Surviving
  mutations** (severity-ranked table, `🟥 critical / 🟧 high / 🟨 medium / ⬜ low`),
  **Debt-delta**, **Tests with no assertions**, and the advisory
  **Slop / Security / Convention** tables with their 0–100 scores. A fully
  clean run gets a single "nothing found on the changed surface" line.
- **Failure mode:** best-effort — see the exit-code contract above.

## Progression snapshot (`progression` record)

`slop-gate progression --plane-url` posts one JSON line per resolve, tagged
`record_type: "progression"`. Identity is the usual `(repo, head_sha, run_id)`
idempotency key, so a retried CI step is a no-op.

```json
{
  "record_type": "progression",
  "repo": "acme/api", "pr": null,
  "head_sha": "d72c6f9…", "run_id": "1899", "actor": "octocat",
  "timestamp_unix": 1789748861,
  "title": "Acme API — 2026 H1", "season": "2026 H1",
  "totals": {
    "xp_earned": 20, "xp_total": 50, "pct_bp": 4000,
    "nodes_total": 2, "nodes_done": 1, "nodes_in_progress": 1,
    "nodes_available": 0, "nodes_locked": 0,
    "parts_total": 2, "parts_done": 1,
    "commits": 2, "prs": 2,
    "level": 1, "level_floor_xp": 0, "next_level_xp": 50
  },
  "nodes": [
    {
      "id": "ingest", "title": "Ingest pipeline",
      "requires": [], "state": "done",
      "xp_earned": 20, "xp_total": 20, "pct_bp": 10000,
      "parts": [
        {
          "id": "validate", "title": "Reject a malformed batch at the door",
          "done": true, "xp": 20,
          "have_commits": 1, "need_commits": 1, "have_prs": 1, "need_prs": 0,
          "evidence": [
            { "sha": "08cf630…", "subject": "feat: validate (#1)", "pr": 1, "timestamp_unix": 1789748810 }
          ]
        }
      ],
      "commits": 1, "prs": [1],
      "first_activity_unix": 1789748810, "last_activity_unix": 1789748810,
      "col": 0, "row": 0
    },
    {
      "id": "api", "title": "Public API",
      "requires": ["ingest"], "state": "in_progress",
      "xp_earned": 0, "xp_total": 30, "pct_bp": 0,
      "parts": [
        {
          "id": "routes", "title": "Routes and their contract",
          "done": false, "xp": 30,
          "have_commits": 1, "need_commits": 0, "have_prs": 1, "need_prs": 2,
          "evidence": [
            { "sha": "d72c6f9…", "subject": "feat: routes (#2)", "pr": 2, "timestamp_unix": 1789748810 }
          ]
        }
      ],
      "commits": 1, "prs": [2],
      "first_activity_unix": 1789748810, "last_activity_unix": 1789748810,
      "col": 1, "row": 0
    }
  ],
  "tool_version": "0.5.0"
}
```

Field notes:

- **`pct_bp`** — basis points, on the record and on every node. Integer so every
  consumer rounds the same way.
- **`state`** — `locked` | `available` | `in_progress` | `done`. A node is
  `done` only when every part closed **and** every requirement is `done`.
- **`col` / `row`** — the emitter's layout, carried rather than recomputed, so
  the console's canvas and the SVG in the repository draw the same tree in the
  same arrangement.
- **`evidence`** — at most three commits per part, newest first. The `have_*`
  counts are not capped, so a part can report 40 commits and show three.
- **`totals.nodes_total`** must equal the number of nodes sent; a plane rejects
  a rollup that disagrees with its own node list.

Consumed by `POST /v1/ingest` and served back as `GET /v1/fleet/progression`
(one row per repo, least-advanced first) and
`GET /v1/progression/{owner}/{repo}` (one whole tree; `null` when that repo has
never published one).
