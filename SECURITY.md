# Security Policy

## Reporting a vulnerability

**Do not open a public issue for security problems.**

Report privately via GitHub's **Security Advisories** ("Report a vulnerability"
on the repo's Security tab), or email the maintainer listed on the GitHub
profile. Include: affected version/commit, a description, and a minimal
reproduction if possible.

We aim to acknowledge within **3 business days** and to ship a fix or mitigation
for confirmed, in-scope issues as soon as practical. We'll credit reporters who
want it once a fix is released.

## Supported versions

The latest released minor version receives security fixes. Older versions may
be patched at the maintainer's discretion.

## Scope

In scope — the gate itself:

- The `slop-gate` binary / library and its handling of untrusted input (diffs,
  test output, mutation-engine reports parsed from a PR).
- The composite GitHub Action (`action.yml`): token handling, the authenticated
  binary fetch, and the source-build fallback.
- Telemetry emit (`--metrics-url`): it must never send a Bearer token over
  non-HTTPS (enforced in config validation).

Notably out of scope:

- The third-party **mutation engines** the gate shells out to (cargo-mutants,
  cosmic-ray, Stryker, gremlins, PIT) — report those upstream.
- Findings the gate *reports* (surviving mutants, slop/security patterns) are
  product output, not vulnerabilities in the gate.
- Running the gate against **untrusted code** executes that code's test suite by
  design (mutation testing runs tests). Run it on trusted branches / in an
  isolated runner; see `test-harness/` for a network-isolated container.

## Handling of secrets

The Action masks the release token (`::add-mask::`) and routes the source-build
fallback auth through a scoped git `insteadOf` rewrite so it never reaches a log
line. The gate stores no credentials; telemetry tokens come from the environment
(`METRICS_TOKEN`) at run time only.
