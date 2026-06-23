#!/usr/bin/env bash
# Closed-environment demo of the Slop Filter behavioral merge gate.
#
# Builds a tiny git repo from the theater-test fixture, makes a one-line change
# on the boundary, and runs the gate against that diff — entirely offline. It
# exercises the headline behaviours: surfacing a survivor the suite passed over,
# severity ranking + the severity gate, the debt-delta, advisory mode, and the
# telemetry/analyze trend dashboard.
set -euo pipefail

DEMO=/work/demo
METRICS=/work/metrics.jsonl
rm -rf "$DEMO" "$METRICS"
mkdir -p "$DEMO/src"

git config --global user.email demo@slop.gate
git config --global user.name "slop demo"
git config --global init.defaultBranch main
git config --global commit.gpgsign false

cp /opt/fixtures-template/theater_demo/Cargo.toml "$DEMO/Cargo.toml"
cd "$DEMO"
git init -q

# Base commit: the boundary written one way (age > 17).
cat > src/lib.rs <<'RS'
//! Known-positive fixture: a boundary the theater test never asserts.
pub fn is_adult(age: u32) -> bool {
    age > 17
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn theater_test_exercises_without_asserting() {
        let _ = is_adult(20);
        let _ = is_adult(10);
        assert!(true); // asserts nothing about the boundary
    }
}
RS
git add -A && git commit -qm "base: is_adult via age > 17"

# Head commit: the same behaviour rewritten as age >= 18, so the changed
# surface is exactly the boundary line the gate will mutate.
cat > src/lib.rs <<'RS'
//! Known-positive fixture: a boundary the theater test never asserts.
pub fn is_adult(age: u32) -> bool {
    age >= 18
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn theater_test_exercises_without_asserting() {
        let _ = is_adult(20);
        let _ = is_adult(10);
        assert!(true); // asserts nothing about the boundary
    }
}
RS
git add -A && git commit -qm "head: is_adult via age >= 18"

hr() { printf '\n============================================================\n'; }
run_gate() {
    hr
    echo "### slop-gate $*"
    set +e
    slop-gate --repo . --base HEAD~1 --head HEAD "$@"
    echo "[gate exit code: $?]"
    set -e
}

echo "Slop Filter — closed-environment gate demo (offline)"
echo "Diffing HEAD~1..HEAD of a throwaway repo built from the theater fixture."

# 1) Default blocking run: surfaces the surviving boundary mutant → exit 2.
run_gate

# 2) Advisory mode: reports the same survivor but never blocks → exit 0.
run_gate --advisory

# 3) Severity gate (count budget relaxed so only severity decides):
#    the survivor is HIGH severity, so `critical` passes, `high` blocks.
run_gate --max-survivors 9 --block-on-severity critical
run_gate --max-survivors 9 --block-on-severity high

# 4) Telemetry + the Phase 4 trend dashboard.
slop-gate --repo . --base HEAD~1 --head HEAD --advisory \
    --metrics-file "$METRICS" >/dev/null 2>&1 || true
hr
echo "### slop-gate analyze --metrics-file metrics.jsonl"
slop-gate analyze --metrics-file "$METRICS"

hr
echo "Demo complete. Exit codes above: 0 = passed/advisory, 2 = blocked."
