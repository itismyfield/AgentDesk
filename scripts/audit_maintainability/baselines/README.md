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
