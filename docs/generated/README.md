# Generated Docs Drift Policy

Generated docs drift means a committed generated report no longer matches the
current source tree. Drift is allowed to surface as a warning on pull requests;
it should not block ordinary feature work unless the change is specifically
about the generated report or its generator.

## Current Policy

- PR CI in `.github/workflows/ci-pr.yml` treats inventory-doc drift from
  `python3 scripts/generate_inventory_docs.py --check` as warning-only. The
  warning points contributors to the weekly refresh workflow or to a local
  regeneration command.
- `python3 scripts/audit_maintainability.py --check` is still allowed to fail
  for configured hard or baseline maintainability gates. The warning-only drift
  policy applies to the committed markdown report freshness, not to new
  maintainability violations.
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

## When to Regenerate Locally

Regenerate locally when the PR itself changes route or module structure and the
updated report would help reviewers inspect the diff, or when the PR changes the
generator, maintainability rules, baselines, or report wording.

Commands:

- `python3 scripts/generate_inventory_docs.py`
- `python3 scripts/audit_maintainability.py --write-report`

Search terms: generated docs drift, generated-docs drift, inventory docs drift,
maintainability audit drift.
