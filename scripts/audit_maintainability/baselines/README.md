# Maintainability Baselines

This directory stores committed no-regression baselines for audit checks that
are not ready to require zero findings.

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

Do not raise this baseline to admit new backflow; a PR that raises it together
with a new reference bypasses the gate, so reviewers reject any increase. Call
the canonical service path instead of a route re-export, or pass server-owned
dependencies in from the assembly layer.
