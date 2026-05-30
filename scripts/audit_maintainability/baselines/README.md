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

## Service/Server Backflow

`service_server_backflow.json` records the current `src/services/**`
references to `crate::server` or `super::server`. CI allows the existing
server-layer backflow but fails when:

- total service/server backflow findings increase above `total_count`
- any file has more findings than its committed per-file `count`
- a new file appears with a service/server backflow finding

To lower the baseline after moving shared types/helpers out of `src/server`:

1. Run `python3 scripts/audit_maintainability.py --format json`.
2. Read `.checks.service_server_backflow.findings`.
3. Update `service_server_backflow.json` so `total_count` and `files` match
   the new lower current findings.
4. Run `python3 scripts/audit_maintainability.py --check`.

Do not raise this baseline to admit new backflow. Move shared DTOs, routing
helpers, websocket emitters, or cluster helpers below `src/services` or pass
server-owned dependencies into services.
