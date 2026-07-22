#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────────────
# git-merge-regen-inventory.sh — custom git merge driver for generated inventory
# docs (#4724 slice B).
#
# Problem it solves: the tracked route and worker inventory docs are generated
# files. When two branches both change the SAME summary row, a plain 3-way merge
# can leave conflict markers — and with the inventories
# regenerated on every prod-line change, concurrent PRs collide constantly →
# O(N^2) serial rebases.
#
# Strategy (merge-file first, regenerate only on real conflict):
#   1. Attempt git's normal line-level 3-way merge (`git merge-file`). When the
#      two sides touched DIFFERENT rows this merges cleanly and the result is
#      identical to what git would have produced without this driver — so we
#      never regress independent, non-colliding inventory edits.
#   2. Only when that 3-way genuinely CONFLICTS do we discard it and REGENERATE
#      the inventory from the working tree, which removes the conflict markers so
#      the merge/rebase is not blocked. NOTE: under `ort` the colliding module's
#      merged source is not reliably materialized in the working tree at driver
#      time, so the regenerated counts for that module may be momentarily stale.
#      This is intentional and safe: the driver is a best-effort auto-resolver,
#      and the AUTHORITATIVE correctness check is the server-side CI freshness
#      gate (scripts/ci-script-checks.sh regenerates then checks tracked-doc
#      diffs, a hard PR failure on drift). Any residual drift the driver
#      produces is caught there — exactly as a stale manual resolution would be —
#      so it can never reach `main`. See docs/agent-maintenance/merge-driver-
#      inventory.md ("Correctness backstops"). We deliberately do NOT regenerate
#      on the clean-merge path: measured, an in-driver regen there reads stale
#      one-sided sources and would corrupt/deadlock correct independent merges.
#
# git invokes it (see scripts/setup-merge-drivers.sh) as:
#   bash scripts/git-merge-regen-inventory.sh %O %A %B %P
#     %O = ancestor version (temp path)
#     %A = ours/current version (temp path) — the driver MUST leave the merge
#          result here; git reads it back as the resolved content
#     %B = theirs version (temp path)
#     %P = pathname in the repo (e.g. docs/generated/route-inventory.md)
#
# Fail-closed contract: if a conflict needs regeneration and regeneration fails
# for ANY reason, exit non-zero. git then leaves the normal conflict in place
# for a human. The driver never emits partially-generated or garbage content.
# ──────────────────────────────────────────────────────────────────────────────
set -uo pipefail

ANCESTOR="${1:-}"   # %O
CURRENT="${2:-}"    # %A — write the resolved content here
OTHER="${3:-}"      # %B
PATHNAME="${4:-}"   # %P — repo-relative path being merged

if [ -z "$ANCESTOR" ] || [ -z "$CURRENT" ] || [ -z "$OTHER" ] || [ -z "$PATHNAME" ]; then
  echo "git-merge-regen-inventory: expected %O %A %B %P arguments" >&2
  exit 1
fi

PYTHON="${PYTHON:-python3}"

# Step 1: try git's standard line-level 3-way merge into %A (ours). rc 0 means a
# clean merge — keep it verbatim (matches default git behaviour, no regression).
if git merge-file -q -p "$CURRENT" "$ANCESTOR" "$OTHER" >"$CURRENT.regen-merged" 2>/dev/null; then
  mv "$CURRENT.regen-merged" "$CURRENT"
  echo "git-merge-regen-inventory: clean 3-way merge for $PATHNAME (no regeneration needed)"
  exit 0
fi
rm -f "$CURRENT.regen-merged"

# Step 2: real conflict → regenerate from the working tree (the colliding
# module's source has already been content-merged into the tree at this point).
if ! REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"; then
  echo "git-merge-regen-inventory: not inside a git working tree" >&2
  exit 1
fi

GENERATOR="$REPO_ROOT/scripts/generate_inventory_docs.py"
if [ ! -f "$GENERATOR" ]; then
  echo "git-merge-regen-inventory: generator not found at $GENERATOR" >&2
  exit 1
fi

if ! regen_output="$(cd "$REPO_ROOT" && "$PYTHON" "$GENERATOR" 2>&1)"; then
  echo "git-merge-regen-inventory: regeneration failed; leaving conflict for a human" >&2
  printf '%s\n' "$regen_output" >&2
  exit 1
fi

REGENERATED="$REPO_ROOT/$PATHNAME"
if [ ! -f "$REGENERATED" ]; then
  echo "git-merge-regen-inventory: regenerated file missing at $REGENERATED" >&2
  echo "(is $PATHNAME actually emitted by generate_inventory_docs.py?)" >&2
  exit 1
fi

if ! cp "$REGENERATED" "$CURRENT"; then
  echo "git-merge-regen-inventory: failed to copy regenerated $PATHNAME into place" >&2
  exit 1
fi

echo "git-merge-regen-inventory: conflict in $PATHNAME auto-resolved by regeneration"
exit 0
