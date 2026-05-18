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

EOF
