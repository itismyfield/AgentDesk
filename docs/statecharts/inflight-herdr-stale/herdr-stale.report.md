Code SHA: bdf9ad22279dee9812e82255a559b4edff38da06 (base) with the #8343 H stale-check change.

# Herdr inflight row: stale-age verdict against the recorded pane

`InflightHerdrStale.tla` follows one Herdr turn's inflight row through turn end, pane close or
retire, a Herdr outage, a replacing server that lists no pane, one dcserver restart, the owner's
finalize, the Herdr row-end pass and the stale-age check in `inflight/removal.rs`. A row the stale
check judges stale is hidden from the scan loaders; the boot reaper deletes it on the next boot.

The check reads the recorded pane through `HerdrRegistry::pane_presence`: one complete
`session.snapshot` of the endpoint that serves the row locator's node and session. That is the
same read `agentdesk herdr status` starts with. A pane missing from a snapshot that lists other
panes reads `Missing`. A listed pane, an empty list, an incomplete answer, an unreachable server
or no single local endpoint keeps the row. The loader gate already keeps current-generation rows,
and a path-less row stays visible to the Herdr row-end pass that settles it.

| Config | Mode | Result |
| --- | --- | --- |
| HerdrStaleBefore | main: a non-tmux host always keeps the row | RED `ClosedPaneRowRetires` ([report](herdr-stale-before.report.md)) |
| HerdrStaleAfter | this change | GREEN, all rules ([report](herdr-stale-after.report.md), [chart](herdr-stale-after.chart.mmd)) |
| HerdrStaleNoDead | mutant: `Missing` never reads dead | RED `ClosedPaneRowRetires` ([report](herdr-stale-no-dead.report.md)) |
| HerdrStaleNoEmptyGuard | mutant: an empty list reads `Missing` | RED `NoLiveTurnRowHidden` ([report](herdr-stale-no-empty-guard.report.md)) |
| HerdrStaleNoPathGuard | mutant: path-less rows judged too | RED `NoPathlessHidden` ([report](herdr-stale-no-path-guard.report.md)) |
| HerdrStaleAfterIdleGap | this change, known gap | RED `IdlePaneRowRetires` ([report](herdr-stale-after-idle-gap.report.md)) |

Rules: `NoLiveTurnRowHidden` (safety: a running turn's row is never hidden or deleted),
`NoPathlessHidden` (safety), `ClosedPaneRowRetires` (liveness: once the pane is gone the row
leaves the stale scan unless only a replacing server answers).

Known gaps, not closed here:
- A prior incarnation's ended turn whose pane stays open (idle) keeps its row (`IdlePaneRowRetires`).
- Hiding is not deletion. Single-row readers (queue hold, idle transcript relay, `/clear`, outbox)
  still see a hidden row until the boot reaper removes it.
- A replacing server that lists no pane keeps the row; so does a Herdr server with no pane at all.

Only the after chart is committed. The other configs draw about 200 states, and their charts can
be regenerated with the README command.
