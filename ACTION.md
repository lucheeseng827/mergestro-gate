# Using the Slop Filter GitHub Action

A copy-paste guide to running the behavioral merge gate as a GitHub Action on
your PRs. For *what* the gate does and how to read its output, see
[`README.md`](./README.md) / [`GUIDE.md`](./GUIDE.md); this page is just wiring.

- **Gating your own private monorepo?** You don't need this Action at all — build
  from source in CI (no token, no release). Jump to [Same-repo](#same-repo-no-action-needed).
- **Consuming it from another repo?** Start here.

## Quickstart (public action repo)

`.github/workflows/slop-gate.yml`:

```yaml
name: slop-gate
on:
  pull_request:

permissions:
  contents: read
  pull-requests: write        # so the gate can post its comment

jobs:
  behavioral-gate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0       # full history — the merge-base needs it
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2          # warm target/ across runs (big win)
      - uses: lucheeseng827/mergestro-gate@v1
        with:
          max-survivors: "0"   # any survivor blocks
          comment: "true"
```

Then make `behavioral-gate` a **required** check in branch protection — only then
does a blocked verdict actually stop the merge.

### The three setup steps are not optional

| Step | Why it's required |
| ---- | ----------------- |
| `checkout` with `fetch-depth: 0` | The gate diffs against the **merge-base** of your PR; a shallow clone has no merge-base to resolve. |
| `dtolnay/rust-toolchain` | `cargo-mutants` compiles + re-tests each mutant — it needs a toolchain. |
| `Swatinem/rust-cache` | Caches `target/` so the baseline build isn't re-paid every run. Skipping it works but is much slower. |

The Action itself then installs `cargo-mutants` and the `slop-gate` binary
(prebuilt musl, source-built only as a fallback), resolves the merge-base, and
runs the gate.

## Inputs

| Input | Default | Purpose |
| ----- | ------- | ------- |
| `base-ref` | _(PR base)_ | Branch to diff against. Defaults to the PR's base branch. |
| `max-survivors` | `0` | Survivors tolerated before blocking (`0` = any survivor blocks). |
| `advisory` | `false` | Report but never block (no failed check). |
| `block-on-severity` | _(unset)_ | Block when a survivor reaches a tier (`low`\|`medium`\|`high`\|`critical`), regardless of count. |
| `max-per-function` | `5` | Cap mutants tested per function — the main cost lever. |
| `block-on-zero-assertion` | `false` | Also block when assertion-free tests are found. |
| `debt-budget` | _(unset)_ | Per-PR structural-debt budget (net complexity + duplication + coupling). |
| `block-on-debt` | `false` | Also block when the debt-delta exceeds the budget. |
| `block-on-pattern` | _(unset)_ | Gate a pattern lane — a lane (`slop`/`security`/`convention`/`all`) or rule id (e.g. `hardcoded-secret`, `unknown-crate-import`). Comma-separated; advisory if unset. |
| `jobs` | `4` | Parallel mutant jobs. Raise on bigger runners. |
| `timeout` | `60` | Per-mutant test timeout (seconds). |
| `config` | _(unset)_ | Path to a `slop-gate.yaml` config file (CLI inputs override it). |
| `metrics-file` | _(unset)_ | Append a JSON-Lines validation record (Phase 3 telemetry). |
| `comment` | `true` | Post / update the idempotent PR comment. |
| `version` | `v0.1.0` | Release tag of the prebuilt binary to install. |
| `token` | `GITHUB_TOKEN` | Token used to post the PR comment. |
| `release-token` | _(unset)_ | Read-scoped PAT/App token to fetch the binary when the **action repo is private** — see [Private](#private-action-repo). |

## Exit codes / what blocks

| Code | Meaning |
| ---- | ------- |
| `0` | Passed (or `advisory`): nothing over budget. |
| `2` | **Blocked**: survivors over budget, a chosen severity tier hit, a debt overrun, or an untrustworthy suite. This is what fails the required check. |
| `1` | Operational failure (couldn't diff, engine missing, mutation run failed mid-way). |

The PR comment is best-effort — a token/network hiccup warns but never changes the
verdict or fails the step on its own.

## Roll out advisory → blocking

Don't block on day one:

1. **Advisory.** `advisory: "true"` — comments on every PR, never fails. Watch ~a
   dozen PRs; confirm survivors are real and latency is acceptable.
2. **Blocking, lenient.** Drop `advisory`; set `block-on-severity: "high"` (or a
   non-zero `max-survivors`) so only dangerous misses block.
3. **Blocking, strict.** Tighten to `max-survivors: "0"` once the team trusts it.

## Private action repo

When the repo that *holds* the Action is private, the binary is fetched over the
authenticated GitHub asset API. The consumer repo's default `GITHUB_TOKEN` cannot
read a **different** private repo, so pass a read-scoped PAT or GitHub App token:

```yaml
      - uses: your-org/mergestro-gate@v1
        with:
          release-token: ${{ secrets.MERGESTRO_GATE_READ_TOKEN }}   # read access to the action repo
          max-survivors: "0"
```

`release-token` needs **read** on the action repo's contents/releases only. It also
authenticates the source-build fallback (via a git `insteadOf` rewrite, so it never
appears in a log line). Leave it unset for a public action repo.

Publishing the binary: tag `v*` in the action repo to trigger
[`release.yml`](.github/workflows/release.yml), which builds the static musl
binary + `.sha256` and attaches them to the release. Keep the release (and repo)
private; the Action downloads via the asset API regardless.

## Same-repo (no Action needed)

If the gate lives in the **same** private repo whose PRs you're gating, skip the
Action entirely — build `slop-gate` from source in CI and run it in place. No
release, no registry, no token:

```yaml
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo build --bin slop-gate --release
      - run: |
          MB="$(git merge-base "origin/${{ github.base_ref }}" HEAD)"
          ./target/release/slop-gate --repo . --base "$MB" --head HEAD --skip-preflight --advisory --comment
```

The binary is an in-CI artifact that never leaves the repo. See the
private-builds note in [`GUIDE.md`](./GUIDE.md) for the full topology table.

## Pinning

Pin to the major tag (`@v1`), a release tag (`@v0.1.0`), or a commit SHA — not
`@main` — so a consumer doesn't pick up breaking changes unexpectedly. Bump the
tag (and the `version` input, if you set it explicitly) together.

## Troubleshooting

| Symptom | Cause / fix |
| ------- | ----------- |
| `no base ref` / merge-base error | Missing `fetch-depth: 0` on checkout, or not running on `pull_request`. Add full history or set `base-ref`. |
| Binary download 404s | Private action repo without `release-token` (default `GITHUB_TOKEN` can't read it). Supply a read-scoped PAT/App token. |
| `jq is required` | Self-hosted runner without `jq`. Install it — e.g. `sudo apt-get install -y jq` (Debian/Ubuntu) or `sudo yum install -y jq` (RHEL/Amazon Linux). GitHub-hosted runners already have it. |
| `cargo-mutants not found` / engine errors | Toolchain step missing, or a sandbox blocked the install. Ensure `dtolnay/rust-toolchain` runs first. |
| Runs are slow | Add `Swatinem/rust-cache`; lower `max-per-function`; raise `jobs` on a bigger runner. |
| Mutation step fails on Windows | `cargo-mutants` can't run on Windows (temp-path > `MAX_PATH`). Use a Linux runner; the gate reports this as an operational failure, not a silent pass. |
