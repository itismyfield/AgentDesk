# Generated Docs Drift Policy

Generated docs drift means a committed generated report no longer matches the
current source tree. Drift is allowed to surface as a warning on pull requests;
it should not block ordinary feature work unless the change is specifically
about the generated report or its generator.

## Current Policy

- `module-inventory.md` and `giant-file-registry.md` are deliberately untracked
  checkout-local views. Generate them before reading them; CI does so before
  maintenance checks. Their absence from git is what removes production-line
  churn from concurrent PR merges (#4724).
- PR CI in `.github/workflows/ci-pr.yml` does not hard-block solely on
  inventory-doc markdown freshness drift.
- `ci-pr.yml` and `ci-main.yml` run `scripts/ci-script-checks.sh`, which calls
  `python3 scripts/generate_inventory_docs.py` in the CI workspace. That keeps
  downstream maintainability checks on the current generated view, but generic
  committed markdown freshness drift is not the hard failure and does not need
  to be committed in unrelated PRs.
- `ci-nightly.yml` regenerates all inventory views, then reports drift only for
  the tracked outputs: `ARCHITECTURE.md`, route inventory, and worker inventory.
  The warning points contributors to the weekly refresh workflow or to a local
  regeneration command.
- `python3 scripts/audit_maintainability.py --check` is still allowed to fail
  for configured hard or baseline maintainability gates. The warning-only drift
  policy applies to the committed markdown report freshness, not to new
  maintainability violations.
- `scripts/generate_inventory_docs.py` is still allowed to hard-fail on
  source-of-truth invariants such as giant-file registry drift, missing required
  registry metadata, or top-level architecture map parse errors. Those are not
  generic generated-doc markdown freshness failures.
- Do not re-introduce a hard PR gate solely because generated docs drifted.

## Rationale

The previous hard gate created CI backlog because contributors had to refresh
large generated reports even when their changes did not depend on those reports.
That cost was disproportionate to the review value. Generated docs are useful
for navigation and periodic architecture review, but most PRs should not have to
carry mechanical report churn.

## Refresh Path

The weekly `Regen inventory docs` workflow in `.github/workflows/regen-docs.yml`
runs every Monday at 01:00 UTC (10:00 KST). It runs
`python3 scripts/generate_inventory_docs.py`, detects drift in `ARCHITECTURE.md`
and `docs/generated`, and opens a maintenance PR when inventory docs changed.
That maintenance PR is intentionally review-and-merge, not auto-merge.

`docs/generated/maintainability-audit.md` is refreshed intentionally with
`python3 scripts/audit_maintainability.py --write-report` when maintainability
rules, baselines, or report text change. CI also uploads the structured audit
artifact from the current run, so stale committed markdown should not be treated
as a reason to hard-fail unrelated PRs.

`scripts/audit_legacy_sqlite_sunset.py` remains available as an on-demand
historical cleanup audit. Its markdown output is intentionally not committed
under `docs/generated/` because the SQLite sunset surface is retired and a
checked-in snapshot quickly becomes stale. For issue work that touches the
SQLite sunset contract, run the script locally and include the current summary
in the PR instead of reintroducing a generated report.

## When to Regenerate Locally

Regenerate locally when the PR itself changes route or module structure and the
updated report would help reviewers inspect the diff, or when the PR changes the
generator, maintainability rules, baselines, or report wording.

Commands:

- `python3 scripts/generate_inventory_docs.py`
- `python3 scripts/audit_maintainability.py --write-report`
- `python3 scripts/audit_legacy_sqlite_sunset.py --root . --format markdown --top 20`

Search terms: generated docs drift, generated-docs drift, inventory docs drift,
maintainability audit drift.
