# Maintainability Baselines

This directory stores committed no-regression baselines for audit checks that
are not ready to require zero findings.

## Low-level Host Accessors

`generated/low_level_accessors/*.json` freezes the production call/reference
sites for raw endpoint, launch-selection and boot-inventory access outside
their owners. Each rule records its accessor and exact owner paths. Output
membership stays with `scripts/check_tui_o_writer_census.py`; naming and
reaction policy migrations keep their existing guards.

The scanner follows the non-test Rust module graph, including inline cfg
gates, and retains literal values and operator order in each call hash.
Comments, formatting, imports and definitions do not create sites. Calls and
function-value references through import aliases do. A site is identified by
rule, source path, enclosing function and normalized call hash, with a count
for duplicate occurrences. Adding a site fails even when another site is
removed in the same change.

Run the audit with `python3 scripts/audit_maintainability.py --check`.
After removing calls, regenerate the approval files with:

```sh
python3 scripts/audit_maintainability/checks/low_level_accessor_baseline.py --reduce
python3 scripts/audit_maintainability/checks/low_level_accessor_baseline.py --check-baseline
```

For a pure file/function rename, use `--relabel` instead of `--reduce` and
retain its printed old/new mapping with the review evidence. Relabeling
requires the exact same `(rule, normalized call hash)` multiset, including
each hash's count; it cannot admit a new call or owner scope. `--initialize`
is only for the approved first corpus and refuses to overwrite any existing
rule file. Do not hand-edit or increase these approval files.
The audit also compares approval hash counts against the PR base, so editing
the source and approval files together cannot admit new debt.
Missing baseline files produce one warning and skip this check in partial
repositories. Malformed existing snapshots remain errors.

## Route SRP

`route_srp.json` records the current route SRP finding count and per-file
counts for `route_srp_violations`. CI allows existing findings but fails when:

- total route SRP findings increase above `total_count`
- any file has more findings than its committed per-file `count`
- a new file appears with a route SRP finding

To lower the baseline after a route refactor PR:

1. Run `python3 scripts/audit_maintainability.py --format json`.
2. Read `.checks.route_srp_violations.findings`.
3. Update `route_srp.json` so `total_count` and `files` match the new lower
   current findings.
4. Run `python3 scripts/audit_maintainability.py --check`.

Do not raise this baseline to admit new route SRP debt. Move SQL/domain work
out of route files instead.

## Server-Layer Backflow

`service_server_backflow.json` records production references to `crate::server`
(including `super::server`, use groups, and crate-root glob paths) from modules
outside the assembly layer (`server`, `cli`, `launch`, `bootstrap`). Test files
and inline `#[cfg(test)]` items are not counted. `--check` fails when:

- total server backflow findings increase above `total_count`
- any file has more findings than its committed per-file `count`
- a new file appears with a server backflow finding

To lower the baseline after moving shared types/helpers out of `src/server`:

1. Run `python3 scripts/audit_maintainability.py --format json`.
2. Read `.checks.service_server_backflow.findings`.
3. Update `service_server_backflow.json` so `total_count` and `files` match
   the new lower current findings.
4. Run `python3 scripts/audit_maintainability.py --check`.

The same check also rejects any Rust reference (production or test) to the
removed `agentdesk_runtime_root` alias. That rule has no baseline; call
`crate::config::runtime_root()` directly.

Do not raise this baseline to admit new backflow; a PR that raises it together
with a new reference bypasses the gate, so reviewers reject any increase. Call
the canonical service path instead of a route re-export, or pass server-owned
dependencies in from the assembly layer.
