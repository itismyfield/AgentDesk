# Inventory Docs Merge Driver (`regen-inventory`)

> `docs/generated/module-inventory.md` and
> `docs/generated/giant-file-registry.md` are now untracked, checkout-local
> views (#4724). This removes their production-line churn from git merges
> entirely. The `regen-inventory` driver remains only for the two generated
> inventories that are still committed: route and worker. It is a best-effort
> ergonomic auto-resolver; source-of-truth invariants and tracked-doc drift are
> enforced after generation in `scripts/ci-script-checks.sh`.

## One-time developer setup (REQUIRED)

A merge-driver assignment in `.gitattributes` is **inert** until the driver
command is registered in your local git config. Run once per clone:

```bash
bash scripts/setup-merge-drivers.sh
```

This is also invoked by `bash scripts/setup-hooks.sh`, so if you already ran the
hooks bootstrap you are covered. Verify with:

```bash
git config --local --get merge.regen-inventory.driver
# → bash scripts/git-merge-regen-inventory.sh %O %A %B %P
```

## What it covers

`.gitattributes` assigns `merge=regen-inventory` to exactly the two generated
files that remain tracked:

- `docs/generated/route-inventory.md`
- `docs/generated/worker-inventory.md`

The generator also emits `module-inventory.md` and `giant-file-registry.md`, but
`.gitignore` keeps those checkout-local. CI generates them before maintenance
checks consume them, so no merge driver is needed for either file.

**Deliberately excluded** (hand-authored, or emitted by a different generator —
the driver would clobber real content): `docs/generated/README.md`,
`db-file-duplication-audit.md`, `maintainability-audit.md` (written by
`audit_maintainability.py`, not the inventory generator), `pg-audit-checklist.md`,
`policy-db-inventory.md`. `ARCHITECTURE.md` is also excluded because it is a
mixed hand-authored + marker-generated file; regenerating it would not resolve a
conflict located in its hand-authored prose.

## How it works (merge-file first, regenerate only on real conflict)

git invokes a custom merge driver whenever **both** sides modify a covered file.
`scripts/git-merge-regen-inventory.sh %O %A %B %P`:

1. **Tries git's normal line-level 3-way merge** (`git merge-file`). When the two
   sides changed **different** rows (independent modules), this merges cleanly
   and the result is byte-identical to what git would have produced without the
   driver — so independent inventory edits are **never** regressed.
2. **Regenerates only on a genuine conflict.** A doc row collides when both sides
   changed the **same** module (or a shared summary line). On that path the
   driver runs `python3 scripts/generate_inventory_docs.py` and takes its output,
   removing the conflict markers. The regenerated file is written over git's `%A`
   (result) path and the driver exits 0.

This two-step design is deliberate: an unconditional regenerate would be *worse*
than the default merge for the common independent-edit case, because under git's
`ort` strategy a source file changed on only one side is written to the working
tree *after* the driver runs. Delegating independent rows to `git merge-file`
sidesteps that entirely.

### Honest limits (why the driver is best-effort, not authoritative)

The driver is an **ergonomic auto-resolver**, not a correctness oracle. Two
empirically-verified `ort` facts bound what it can guarantee:

- On the **regenerate path**, `ort` does **not** reliably materialize the
  colliding module's merged source into the working tree before invoking the
  driver, so the regenerated counts for that module can be momentarily stale.
- On the **clean-merge path**, in-driver regeneration would be actively harmful,
  so we do **not** self-validate there. Measured directly: for two branches that
  change *different* modules (a correct, independent-row merge), `git merge-file`
  yields the correct `app_state=49`, but a regenerate at driver time yields a
  **stale** `app_state=47` — because the other side's one-sided source change is
  not yet in the working tree. Comparing the two would flag a *correct* merge as
  a mismatch and either fail-closed (re-introducing the conflict the driver
  exists to remove) or overwrite the correct result with the stale one. So the
  clean 3-way result is taken verbatim.

What the driver guarantees is narrow and sufficient: it **never leaves conflict
markers on churn** (eliminating the O(N²) manual-resolution tax), and it **never
emits content that is worse than a bad manual resolution would be today** — any
residual drift it produces is caught by the same CI gate that catches a stale
hand-resolved doc.

**Fail-closed:** if the conflict path's regeneration fails, the driver exits
non-zero, leaving the ordinary conflict in place for a human. It never emits
partially-generated content.

## Correctness backstops (authoritative vs convenience)

- **Authoritative, server-side — generation plus focused tracked diff.**
  `scripts/ci-script-checks.sh` (run by the required *Script checks* job in
  `.github/workflows/ci-pr.yml`) first runs `python3
  scripts/generate_inventory_docs.py`. Generation hard-fails source-of-truth
  violations such as an unregistered giant or invalid registry metadata. CI
  then runs `git diff --exit-code` for `ARCHITECTURE.md`, route inventory, and
  worker inventory, the three generated outputs that remain tracked.
  `check_agent_maintenance_docs.py` consumes the freshly generated, untracked
  module inventory and keeps frozen-surface membership/threshold checks active.
- **Local convenience — the pre-push hook.** `.githooks/pre-push` regenerates
  inventory docs when `src` changed and blocks/amends before push, so most
  tracked drift never leaves the machine. It is a convenience only: it is
  skippable with `git push --no-verify` and requires
  `core.hooksPath=.githooks` (set by `scripts/setup-hooks.sh`). CI remains the
  correctness authority.

## CI note

CI needs no *driver* registration: `.github/workflows/ci-pr.yml` uses
`actions/checkout@v4` and never performs a local `git merge`, and GitHub's
server-side PR merge / merge-queue does **not** honor custom `.gitattributes`
merge drivers. The driver's value is entirely local (developer rebases/merges).
The server-side gate does not rely on the driver: it regenerates from the merged
source tree, validates inventory invariants, and rejects drift in the remaining
tracked outputs.
