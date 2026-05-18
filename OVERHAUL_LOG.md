# Dashboard Overhaul Log

**Branch:** `wt/dashboard-overhaul-20260518` (based on `origin/main`)
**Worktree:** `/Users/itismyfield/.adk/release/worktrees/dashboard-overhaul-20260518`
**Started:** 2026-05-18 23:48 KST
**Deadline:** 2026-05-19 07:00 KST
**Operator:** adk-dashboard (autonomous /loop)

## Goals
Improve the AgentDesk dashboard along 8 quality dimensions:

1. **Glanceability** — health visible in <5s (status badges, sparklines, color-coded summaries)
2. **Real-time reliability** — WS freshness indicator, separated loading/empty/error states
3. **Action-in-place** — operational actions available in context (no page hops)
4. **Information hierarchy** — card sizing/placement reflects priority
5. **Responsive (mobile first-class)** — mobile-specific IA, not a desktop shrink-down
6. **Performance** — virtualization, code splitting, reduced re-renders
7. **Design system consistency** — tokenized colors/spacing, same-meaning = same-visual
8. **Observability** — timestamps everywhere, links to logs/sources, traceability

## Baseline (captured pre-overhaul)
- React 19 + Vite 6 + Tailwind 4 + React Router 7 + React Query + Pixi.js
- 11 primary routes, 3 contexts (Office/Settings/Kanban), `useDashboardSocket` hook
- Mobile breakpoint at 900px; mobile layout via overrides (not mobile-first CSS)
- ~155 TSX components, 6 pages >590 LOC, 1875 inline styles, 43 test files
- Existing design tokens: `src/theme/statusTokens.ts` (kanban-focused); CSS `--th-*` custom properties
- WS hook exposes only `wsConnected` boolean — no event freshness signal

## Rounds

### Round 1 — 2026-05-18 23:48~23:57 KST
**Focus:** Design system (7) + Real-time reliability (2) + Glanceability (1) — foundation primitives.

**Changes:**
- `theme/statusTokens.ts`: added `SYSTEM_HEALTH_TONES` (healthy/warning/critical/idle/info/unknown) + `getSystemHealthTone()`. The kanban-only token file now also speaks a generic system-health language.
- `components/common/StatusBadge.tsx`: new reusable badge — accepts a named tone or a custom `StatusTone`, supports xs/sm/md sizes and a live-pulse dot. 4 unit tests.
- `components/common/FreshnessIndicator.tsx`: new "n초 전" indicator with healthy→warning→critical escalation, self-ticking, ms/s/ISO timestamp tolerant, `데이터 없음` for null. 6 unit tests.
- `styles/main.css`: added `@keyframes adkStatusPulse` and `prefers-reduced-motion` handling.
- `app/useDashboardSocket.ts`: now exposes `lastEventTs` so consumers can wire `FreshnessIndicator` to the live WS stream.

**Verification:** 10/10 new tests pass; full `npm run build` succeeds in 6.4s.

**Next:** Wire `FreshnessIndicator` into the WS connection chip + replace bespoke status pills across HomeOverview/Ops with `StatusBadge`.

### Round 2 — 2026-05-19 00:29~00:35 KST
**Focus:** Real-time reliability (2) + Glanceability (1) + Design-system consistency (7).

**Changes:**
- `useDashboardSocket.lastEventTs` propagated through `App.tsx → AppShell → AppShellRoutes → HomeOverviewPage` as `wsLastEventTs` prop.
- `HomeOverviewPage` header chip: replaced the bespoke ws-dot + "all systems normal" span with `StatusBadge` (`tone="healthy"|"critical"`, pulse when healthy) + inline `FreshnessIndicator` showing "n초 전" with 45s warn / 180s critical thresholds. The header now answers "is the screen live?" at a glance.
- `DashboardHomeOverview` (the larger overview surface) `systemState`: refactored from ad-hoc `{label,color,pulseColor}` to `{label, tone: SystemHealthTone}` and rendered via `StatusBadge`. Three branches (warning / info / healthy) now speak the same visual language as the rest of the system.
- Net: 2 places that previously spoke their own visual language now share the system-health vocabulary; "stale data" is now an explicit, escalating signal instead of an invisible failure mode.

**Verification:** `npm run build` ✓ in 3.6s. No new tests needed (UI surfaces; existing tests untouched).

**Next:** continue replacing bespoke pills in Ops/Agents/Kanban surfaces, and/or wire `FreshnessIndicator` into per-widget refresh signals (HealthWidget, RateLimitWidget).

### Round 3 — 2026-05-19 00:59~01:05 KST
**Focus:** Ops surface — Design-system consistency (7) + Glanceability (1) + Real-time reliability (2).

**Changes:**
- `OpsPageModel.opsToneToHealth()`: new mapper from Ops's local `info|warn|danger|success` tone vocabulary to the shared `SystemHealthTone`. Lets Ops surfaces opt into `StatusBadge` incrementally without touching the dozens of in-table `chipClassFromTone` callers.
- `OpsConnectionPanel`:
  - Header now has an inline `FreshnessIndicator` (20s warn / 60s critical) wired to `lastHealthAt`. The operator can immediately tell whether the Ops panel is showing current state or a stale snapshot.
  - "WS LIVE/DISCONNECTED" chip → `StatusBadge tone={healthy|critical}` with pulse on healthy.
  - "HOT/BOOT" prompt-retention chip → `StatusBadge` via `opsToneToHealth(promptRetentionTone)`.
- `OpsPageView`: now forwards the already-tracked `lastSuccessAt` as `lastHealthAt` to the connection panel.

**Verification:** `npm run build` ✓ in 3.8s. No tests in the affected files; primitives' tests still cover behavior.

**Next:** continue cleaning bespoke chips in OpsPageView itself (header, recovery signal rows, runtime rows) and pull the same `opsToneToHealth` adapter into other Ops sections — or wire HealthWidget/RateLimitWidget freshness next round.

### Round 4 — 2026-05-19 01:27~01:30 KST
**Focus:** HealthWidget — Glanceability (1) + Real-time reliability (2) + Design-system consistency (7).

**Changes:**
- `dashboard/HealthWidget.tsx`:
  - Header status chip ("HEALTHY/DEGRADED/UNHEALTHY") and poll-state chip ("Live/Stale/Error/Loading/Empty") now render through `StatusBadge`. New `healthLevelToTone()` and `pollStateToTone()` helpers map this widget's local vocabulary onto `SystemHealthTone`.
  - "Updated HH:MM:SS" line replaced with `FreshnessIndicator` (45s warn, `HEALTH_STALE_AFTER_MS=75s` critical), so escalation aligns with the widget's existing stale threshold.
  - Degraded-reason chips also adopt `StatusBadge` (warning/critical tone).
  - Loading state pulses the poll-state badge for visible motion while syncing — silent loading was a regression-mode previously.
- `formatUpdatedAt` helper removed (now obsolete); `localeTag` prop kept as optional for caller stability.

**Verification:** `npm run build` ✓ in 3.6s. 16/16 tests pass (incl. all HealthWidget helper tests untouched).

**Net effect:** the operations Health card now speaks the same visual + freshness language as the home overview and Ops connection panel. Three places that drove user trust now share one vocabulary.

**Next:** apply the same treatment to RateLimitWidget + BottleneckWidget, or pivot to (g) explicit loading/empty/error surfaces for widgets that currently show silent blank states.

### Round 5 — 2026-05-19 01:55~02:00 KST
**Focus:** Ops page sweep — Design-system consistency (7) + Glanceability (1) + Real-time reliability (2).

**Changes:**
- `OpsPageView.tsx`: all 6 remaining `chipClassFromTone` callsites + the inline pulse-dot WS chip + the "STALE" warn chip + the "Updated …" plain chip + the recovery / provider / severity badges → `StatusBadge` (via `opsToneToHealth`).
- Header "Updated HH:MM:SS" plain-chip replaced with `FreshnessIndicator` (thresholds tied to existing `STALE_AFTER_MS`).
- Dead local `lastUpdatedLabel` + unused `formatUpdatedAt` import removed.

**Net effect:** the entire Ops page now uses the same visual language. From the operator's perspective: WS status, health status, recovery, provider count, stale flag, bottleneck severity, recovery duration all read with one tone vocabulary. The "Updated X seconds ago" signal now escalates instead of looking constant.

**Verification:** `npm run build` ✓ in 3.6s.

**Next:** RateLimitWidget + BottleneckWidget tokenization, or pivot to (d) AppShell extraction / (e) HomeOverviewPage decomposition for performance + maintainability.

### Round 6 — 2026-05-19 02:23~02:27 KST
**Focus:** Observability (8) + Real-time reliability (2) — explicit loading/empty/error surfaces.

**Changes:**
- New primitive `components/common/WidgetState.tsx`: unified loading / empty / error / stale surface. Auto-maps each kind to a `SystemHealthTone` (info / idle / critical / warning) with appropriate icon, `role="status"`/`role="alert"`, `aria-live`, and an optional action slot. Compact mode for inline use.
- 5 unit tests cover the kind→tone mapping, accessibility roles, and tone override.
- `BottleneckWidget`:
  - Bespoke red error block → `WidgetState kind={"stale"|"error"}` so the operator sees whether they are looking at a stale snapshot vs total failure.
  - "Scanning bottlenecks" plain text → `WidgetState kind="loading"`.
  - **New empty state** wired explicitly — previously, if `cards.length === 0` and not loading and no error, the widget rendered three empty columns silently. Now it surfaces an explicit "no kanban cards in scope" message.
  - Alerts pill → `StatusBadge` (healthy/warning/critical based on count) with pulse on ≥5 alerts.

**Net effect:** introduces a reusable widget-state primitive that future rounds can wire into RateLimitWidget, HealthWidget metrics, AutoQueueHistoryWidget etc. BottleneckWidget no longer fails silently; its alert count now reads as a tone-coded badge instead of a one-style danger pill regardless of severity.

**Verification:** 5/5 new tests pass; full primitives suite still green; `npm run build` ✓ in 3.5s.

**Next:** apply `WidgetState` to RateLimitWidget + at least one more widget; or pivot to (c)/(d) — AppShell or HomeOverviewPage decomposition.

EOF
