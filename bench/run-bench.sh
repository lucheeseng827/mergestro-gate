#!/usr/bin/env bash
# COGS baseline for the Slop Filter behavioral merge gate.
#
# Establishes three things, all reproducible and offline:
#   1. Orchestration tax  — how much wall-clock slop-gate adds on top of the
#      raw `cargo-mutants` engine it wraps (gix diff + cap + parse + verdict).
#   2. Cost breakdown     — where the time goes (enumerate vs build+test vs
#      orchestrate) and how it scales with diff size / mutant count.
#   3. Cross-language anchor — per-mutant wall-clock for the same logical change
#      under cargo-mutants (Rust) vs mutmut (Python) vs Stryker (JS).
#
# The LLM-reviewer comparison is a documented reference row in BENCHMARK.md, not
# measured here (no model is in the loop — see README).
#
# Knobs (env):
#   ITERS        iterations per measurement; median reported   (default 5)
#   RUST_SIZES   function counts -> ~3x mutants each            (default "1 5 15")
#   XLANG        run the Python/JS anchor (1/0)                 (default 1)
#   JOBS         cargo-mutants --jobs                           (default nproc)
#   OUT          results dir                                    (default /work/out)
set -euo pipefail

ITERS=${ITERS:-5}
RUST_SIZES=${RUST_SIZES:-"1 5 15"}
XLANG=${XLANG:-1}
JOBS=${JOBS:-$(nproc 2>/dev/null || echo 4)}
OUT=${OUT:-/work/out}
TIMEOUT=${TIMEOUT:-60}

WORK=/work/bench-tmp
rm -rf "$WORK" "$OUT"; mkdir -p "$WORK" "$OUT"
RESULTS="$OUT/results.json"
: > "$OUT/results.ndjson"

git config --global user.email b@bench.local
git config --global user.name bench
git config --global init.defaultBranch main
git config --global commit.gpgsign false
export GIT_PAGER=cat PAGER=cat

now()  { date +%s.%N; }
dur()  { awk "BEGIN{printf \"%.3f\", $2-$1}"; }            # $1=start $2=end -> seconds
# median of stdin numbers (one per line)
median() { sort -n | awk '{a[NR]=$1} END{ if(NR==0){print "0"; exit} m=int((NR+1)/2); if(NR%2){print a[m]} else {printf "%.3f", (a[m]+a[m+1])/2} }'; }
hr() { printf '%s\n' "------------------------------------------------------------"; }

# ----------------------------------------------------------------------------
# Rust fixture: N boundary functions, each with a theater test. Base writes the
# boundary as `x > 17`, head as `x >= 18`, so every boundary line is in the diff
# and cargo-mutants --in-diff mutates all N (~3 mutants/function).
# ----------------------------------------------------------------------------
gen_rust() {
    local n=$1 dir=$2
    rm -rf "$dir"; mkdir -p "$dir/src"
    cat > "$dir/Cargo.toml" <<EOF
[workspace]
[package]
name = "bench_fixture"
version = "0.1.0"
edition = "2021"
[dependencies]
EOF
    # base
    { echo '#![allow(dead_code)]'
      for i in $(seq 1 "$n"); do
          echo "pub fn f${i}(x: u32) -> bool { x > 17 }"
      done
      echo '#[cfg(test)] mod tests { use super::*;'
      echo '  #[test] fn theater() {'
      for i in $(seq 1 "$n"); do echo "    let _ = f${i}(20); let _ = f${i}(10);"; done
      echo '    assert!(true);'
      echo '  }'
      echo '}'
    } > "$dir/src/lib.rs"
    ( cd "$dir" && git init -q && git add -A && git commit -qm base )
    # head: flip every boundary to >= 18 (same behaviour, line lands in the diff)
    sed -i 's/x > 17/x >= 18/g' "$dir/src/lib.rs"
    ( cd "$dir" && git add -A && git commit -qm head )
}

bench_rust_size() {
    local n=$1
    local dir="$WORK/rust_$n"
    gen_rust "$n" "$dir"
    ( cd "$dir" && git diff HEAD~1 HEAD > diff.patch )

    # mutant count (single enumerate; deterministic)
    local mcount
    mcount=$(cd "$dir" && cargo mutants --in-diff diff.patch --list --json 2>/dev/null | grep -c '"name"' || true)
    [ "$mcount" -gt 0 ] 2>/dev/null || mcount=$(cd "$dir" && cargo mutants --in-diff diff.patch --list 2>/dev/null | grep -c ':' || true)

    local lists raws gates
    lists=$(mktemp); raws=$(mktemp); gates=$(mktemp)
    local k s e
    for k in $(seq 1 "$ITERS"); do
        # warm target/ once before timing so we measure steady-state, not cold build
        if [ "$k" -eq 1 ]; then ( cd "$dir" && cargo build -q 2>/dev/null || true ); fi

        s=$(now); ( cd "$dir" && cargo mutants --in-diff diff.patch --list --json >/dev/null 2>&1 ); e=$(now)
        dur "$s" "$e" >> "$lists"; echo >> "$lists"

        s=$(now); ( cd "$dir" && cargo mutants --in-diff diff.patch --jobs "$JOBS" --timeout "$TIMEOUT" >/dev/null 2>&1 || true ); e=$(now)
        dur "$s" "$e" >> "$raws"; echo >> "$raws"

        s=$(now); ( cd "$dir" && slop-gate --repo . --base HEAD~1 --head HEAD --skip-preflight --advisory --jobs "$JOBS" --timeout "$TIMEOUT" >/dev/null 2>&1 || true ); e=$(now)
        dur "$s" "$e" >> "$gates"; echo >> "$gates"
    done

    local mlist mraw mgate orch
    mlist=$(median < "$lists"); mraw=$(median < "$raws"); mgate=$(median < "$gates")
    orch=$(awk "BEGIN{o=$mgate-$mraw-$mlist; if(o<0)o=0; printf \"%.3f\", o}")
    local pct
    pct=$(awk "BEGIN{ if($mgate>0) printf \"%.1f\", 100*($mgate-$mraw)/$mgate; else print \"0\" }")

    printf '%-8s %-9s %-10s %-10s %-10s %-12s %-8s\n' \
        "$n" "$mcount" "${mlist}s" "${mraw}s" "${orch}s" "${mgate}s" "${pct}%"

    printf '{"kind":"rust","functions":%s,"mutants":%s,"iters":%s,"list_s":%s,"raw_s":%s,"orchestration_s":%s,"gate_s":%s,"overhead_pct":%s}\n' \
        "$n" "$mcount" "$ITERS" "$mlist" "$mraw" "$orch" "$mgate" "$pct" >> "$OUT/results.ndjson"
    rm -f "$lists" "$raws" "$gates"
}

# ----------------------------------------------------------------------------
# Cross-language anchor: same logical change, N functions, theater tests.
# Reports per-mutant wall-clock so different operator sets are comparable.
# ----------------------------------------------------------------------------
bench_python() {
    local n=$1 dir="$WORK/py"
    rm -rf "$dir"; mkdir -p "$dir"
    { for i in $(seq 1 "$n"); do echo "def f${i}(x): return x >= 18"; done; } > "$dir/sut.py"
    { echo 'from sut import *'
      echo 'def test_theater():'
      for i in $(seq 1 "$n"); do echo "    f${i}(20); f${i}(10)"; done
      echo '    assert True'
    } > "$dir/test_sut.py"
    cat > "$dir/setup.cfg" <<EOF
[mutmut]
paths_to_mutate=sut.py
runner=python -m pytest -x -q
EOF
    local s e t mcount
    ( cd "$dir" && mutmut run >/dev/null 2>&1 || true )
    mcount=$(cd "$dir" && mutmut results 2>/dev/null | grep -cE '^[0-9]+-' || true)
    [ "$mcount" -gt 0 ] 2>/dev/null || mcount=$(cd "$dir" && mutmut results 2>/dev/null | grep -oE 'survived|killed|timeout' | wc -l || echo 0)
    local times; times=$(mktemp)
    local k
    for k in $(seq 1 "$ITERS"); do
        ( cd "$dir" && rm -rf .mutmut-cache mutants 2>/dev/null || true )
        s=$(now); ( cd "$dir" && mutmut run >/dev/null 2>&1 || true ); e=$(now)
        dur "$s" "$e" >> "$times"; echo >> "$times"
    done
    t=$(median < "$times"); rm -f "$times"
    local perm; perm=$(awk "BEGIN{ if($mcount>0) printf \"%.3f\", $t/$mcount; else print \"0\" }")
    printf '%-10s %-9s %-10s %-10s\n' "python" "$mcount" "${t}s" "${perm}s"
    printf '{"kind":"python","engine":"mutmut","functions":%s,"mutants":%s,"total_s":%s,"per_mutant_s":%s}\n' \
        "$n" "$mcount" "$t" "$perm" >> "$OUT/results.ndjson"
}

bench_js() {
    local n=$1 dir="$WORK/js"
    rm -rf "$dir"; mkdir -p "$dir/src" "$dir/test"
    { for i in $(seq 1 "$n"); do echo "exports.f${i} = (x) => x >= 18;"; done; } > "$dir/src/sut.js"
    { echo "const s = require('../src/sut');"
      echo "describe('theater', () => { it('runs', () => {"
      for i in $(seq 1 "$n"); do echo "  s.f${i}(20); s.f${i}(10);"; done
      echo "}); });"
    } > "$dir/test/sut.test.js"
    cat > "$dir/stryker.conf.json" <<EOF
{ "\$schema": "./node_modules/@stryker-mutator/core/schema/stryker-schema.json",
  "packageManager": "npm", "testRunner": "mocha",
  "mutate": ["src/sut.js"], "reporters": ["clear-text"],
  "coverageAnalysis": "off", "concurrency": ${JOBS} }
EOF
    # let stryker resolve the globally-installed plugins
    export NODE_PATH; NODE_PATH="$(npm root -g)"
    local s e t mcount
    ( cd "$dir" && stryker run >/tmp/stryker.log 2>&1 || true )
    # clear-text reporter prints one "[Killed]/[Survived]/…" line per mutant
    mcount=$(grep -cE '\[(Killed|Survived|Timeout|NoCoverage|RuntimeError|CompileError)\]' /tmp/stryker.log || true)
    local times; times=$(mktemp)
    local k
    for k in $(seq 1 "$ITERS"); do
        s=$(now); ( cd "$dir" && stryker run >/dev/null 2>&1 || true ); e=$(now)
        dur "$s" "$e" >> "$times"; echo >> "$times"
    done
    t=$(median < "$times"); rm -f "$times"
    local perm; perm=$(awk "BEGIN{ if($mcount>0) printf \"%.3f\", $t/$mcount; else print \"0\" }")
    printf '%-10s %-9s %-10s %-10s\n' "js" "$mcount" "${t}s" "${perm}s"
    printf '{"kind":"js","engine":"stryker","functions":%s,"mutants":%s,"total_s":%s,"per_mutant_s":%s}\n' \
        "$n" "$mcount" "$t" "$perm" >> "$OUT/results.ndjson"
}

# ============================================================================
echo "Slop Filter — COGS baseline   (iters=$ITERS, jobs=$JOBS, offline)"
echo "cargo-mutants: $(cargo mutants --version 2>/dev/null || echo '?')"
hr
echo "[1] Orchestration tax + cost breakdown (Rust — the gate's real engine)"
echo "    list = enumerate · raw = cargo-mutants run · orch = pure wrapper · gate = slop-gate total"
printf '%-8s %-9s %-10s %-10s %-10s %-12s %-8s\n' funcs mutants list raw orch gate overhead
for n in $RUST_SIZES; do bench_rust_size "$n"; done

if [ "$XLANG" = "1" ]; then
    hr
    echo "[2] Cross-language per-mutant cost (same logical change, 5 functions)"
    printf '%-10s %-9s %-10s %-10s\n' lang mutants total per-mutant
    XN=${XN:-5}
    bench_rust_xlang() {
        local dir="$WORK/rust_x"; gen_rust "$XN" "$dir"
        ( cd "$dir" && git diff HEAD~1 HEAD > diff.patch && cargo build -q 2>/dev/null || true )
        local mc; mc=$(cd "$dir" && cargo mutants --in-diff diff.patch --list --json 2>/dev/null | grep -c '"name"' || true)
        local k s e t times; times=$(mktemp)
        for k in $(seq 1 "$ITERS"); do
            s=$(now); ( cd "$dir" && cargo mutants --in-diff diff.patch --jobs "$JOBS" --timeout "$TIMEOUT" >/dev/null 2>&1 || true ); e=$(now)
            dur "$s" "$e" >> "$times"; echo >> "$times"
        done
        t=$(median < "$times"); rm -f "$times"
        local perm; perm=$(awk "BEGIN{ if($mc>0) printf \"%.3f\", $t/$mc; else print \"0\" }")
        printf '%-10s %-9s %-10s %-10s\n' "rust" "$mc" "${t}s" "${perm}s"
        printf '{"kind":"xlang","lang":"rust","engine":"cargo-mutants","functions":%s,"mutants":%s,"total_s":%s,"per_mutant_s":%s}\n' \
            "$XN" "$mc" "$t" "$perm" >> "$OUT/results.ndjson"
    }
    bench_rust_xlang
    command -v mutmut  >/dev/null 2>&1 && bench_python "$XN" || echo "python: mutmut not found — skipped"
    command -v stryker >/dev/null 2>&1 && bench_js     "$XN" || echo "js: stryker not found — skipped"
fi

# fold ndjson into a single results.json array
awk 'BEGIN{print "["} {printf "%s%s", (NR>1?",\n":""), $0} END{print "\n]"}' "$OUT/results.ndjson" > "$RESULTS"
hr
echo "machine-readable results: $RESULTS"
echo "Done."
