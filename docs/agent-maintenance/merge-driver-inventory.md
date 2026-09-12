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
  `scripts/ci-script-checks.sh` (run by the `Script checks runner` job in
  `.github/workflows/ci-pr.yml`; the required *Script checks* context is
  published separately by its fail-closed result mirror) first runs `python3
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

## Integration step: who guarantees freshness, and how

This section owns the *merge-time* half of the contract that the sections above
describe from the git-driver side. It is the named alternative guarantee for
per-PR gates that are deliberately pinned to immutable inputs.

### Per-PR verification is base-pinned on purpose

A required PR context verifies an **immutable input triple**: the event's
`base`, its `head`, and the synthetic merge `candidate` GitHub built from them.
`scripts/giant_file_progress.py` binds its verdict to exactly that triple
(`provenance_matches`) and to the candidate actually checked out, and it does
**not** require `base` to equal the current `origin/main`. So an unrelated main
advance — someone else's PR landing — does not invalidate an already-verified
candidate, and re-running a PR's checks purely because `main` moved is not a
correctness requirement. Branch protection is consistent with this:
`main` sets `required_status_checks.strict = false`, so GitHub itself does not
demand branch currency either.

### The freshness guarantee lives at the merge step

There is **no merge queue** on this repository (no ruleset, and no workflow
carries a `merge_group` trigger), so nothing automatically re-verifies a
candidate against the latest `main`. The guarantee is therefore procedural, and
a single orchestrator owns it end to end:

1. **One integrator.** One orchestrator performs integration and merge for the
   batch. Contributors do not self-merge into `main`.
2. **Build the final candidate.** Immediately before merging, run
   `gh pr update-branch` so the PR's candidate sits on top of current `main`.
3. **Re-pass on that candidate.** Merge only after the **required contexts pass
   on that final candidate**. A green run from an earlier candidate is not a
   merge authorization.
4. **A changed candidate voids the previous evidence.** If `main` moves again
   between step 2 and the merge, the candidate is a different artifact: repeat
   steps 2-3. Never carry the previous candidate's CI result forward as
   approval for the new one.
5. **Reuse review; re-review what integration changed.** Source review of a PR
   whose own commits did not change is reused as-is. Re-review is scoped to
   what integration actually altered: conflict resolutions, and any shared
   contract the merge touched (public signature, DB schema, gate semantics,
   generated-doc surface).

### Why "I just checked `main`" is not the guarantee

Reading `main` immediately before merging leaves the entire lookup→merge window
open; another integrator can land in it. What actually closes the race is step
3 — **the re-pass on the final candidate**, which is the artifact being merged —
not how recently `main` was queried. Evidence supports this because
`scripts/giant_file_progress.py` attributes every record to its own candidate
(`merge_sha`, `merge_first_parent`, `head_sha`, `event_base_sha`,
`candidate_tree`), so a stale result cannot be silently read as covering a
newer candidate.
