# Vendored agentdesk-ui-kit components

<!-- doc-path-refs: external src/kit/ -->

- Source commits per folder in `agentdesk-ui-kit`: `free/` at `3024685b30`, `pro/` at `3024685b30`, `screens/` synced through `3024685b30`, and `illustrations/` synced through `823a81fa00` (`src/kit/<free|pro|screens|illustrations>/*`).
- `free/` files are adapted from affectUI (MIT, see `free/LICENSE.affectui` and the root `NOTICE`). `pro/`, `screens/`, and `illustrations/` files are the kit's clean-room implementations written from public preview specs; they hold no affectUI Pro source.
- Files are copied verbatim — only relative import paths may change. Keep each file's attribution header; do not restyle them here (Tailwind rules do not apply to this folder).
- To update: re-copy the file with `git show <commit>:src/kit/<path>` from the kit commit you move to, bump that folder's commit above, and re-run `npm run build`, `npx vitest run` and the affected e2e specs.
- Per-file imports: `pro/command-menu.tsx` from `b6d2ab1`, used by `components/CommandPalette.tsx` (⌘K); `pro/project-progress-card.tsx` and `pro/gantt-timeline.tsx` from `b33915d68a`, used by `components/campaigns/CampaignOverview.tsx` and `components/campaigns/CampaignRelations.tsx`.
