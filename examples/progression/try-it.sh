#!/usr/bin/env bash
# A whole progression loop on a repository that did not exist a second ago.
#
# Builds a throwaway repo with two components and some history, mines a plan from it, resolves
# that plan back against the same history, and prints the tree. Nothing here touches your repo.
#
#   ./try-it.sh              # run it, keep the scratch repo, print where it is
#   SLOP_GATE=/path/to/bin ./try-it.sh
set -euo pipefail

GATE="${SLOP_GATE:-$(command -v slop-gate || true)}"
if [ -z "$GATE" ]; then
    echo "slop-gate is not on PATH. Build it and try again:" >&2
    echo "    cargo build --release --bin slop-gate" >&2
    echo "    SLOP_GATE=target/release/slop-gate $0" >&2
    exit 1
fi
GATE="$(cd "$(dirname "$GATE")" && pwd)/$(basename "$GATE")"

WORK="$(mktemp -d)"
cd "$WORK"
git init -q -b main .
git config user.email "demo@example.com"
git config user.name "Demo"

# Two components, four commits each, with GitHub's squash marker in the subjects so the
# scaffolder has a PR pattern to find. Real repositories are messier; this is enough to show
# what the mining does.
n=1
for service in auth payments; do
    mkdir -p "services/$service/src" "services/$service/docs"
    for step in one two three four; do
        echo "// $service $step" >>"services/$service/src/lib.rs"
        echo "$service $step" >>"services/$service/docs/notes.md"
        git add -A
        git commit -qm "feat($service): $step (#$n)"
        n=$((n + 1))
    done
done

printf '# demo\n\n<!-- mergestro:progression:start -->\n<!-- mergestro:progression:end -->\n' >README.md
git add -A && git commit -qm "docs: readme with the progression markers (#99)"

echo
echo "=== 1. mine a draft from the history ======================================="
"$GATE" progression init --repo . --out progression.yaml --title "Demo service"

echo
echo "=== 2. what it wrote ======================================================="
sed -n '/^nodes:/,$p' progression.yaml | head -32

echo
echo "=== 3. resolve it and draw it =============================================="
"$GATE" progression --spec progression.yaml --repo . \
    --svg docs/progression.svg --readme README.md

echo
echo "=== what you are looking at ================================================"
cat <<'NOTE'
Everything above the frontier is DONE, because every rule in the draft was written from commits
that already landed. That is the retro-fit problem, stated rather than hidden: the finished part
is the tree's roots, and `next` — the open one, whose rule matches a tag no commit carries — is
the milestone you would replace with what you are actually planning.

A repository with almost no history gets the other shape: under two mined components, `init`
writes a starter plan (foundations -> core -> hardening -> ship) that resolves near zero and
fills in as work lands. See ./README.md.

Next: prompts/01-author-from-a-draft.md, for doing the editing with an assistant.
NOTE

echo
echo "Scratch repo left at: $WORK"
echo "    rm -rf $WORK"
