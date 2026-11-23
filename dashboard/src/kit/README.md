# Vendored agentdesk-ui-kit components

<!-- doc-path-refs: external src/kit/ -->

- Source commits per folder in `agentdesk-ui-kit`: `free/` at `42af99b`, `pro/` at `a11da89` (`src/kit/<free|pro>/*`).
- `free/` files are adapted from affectUI (MIT, see `free/LICENSE.affectui` and the root `NOTICE`). `pro/` files are the kit's clean-room implementations written from public preview specs; they hold no affectUI Pro source.
- Files are copied verbatim — only relative import paths may change. Keep each file's attribution header; do not restyle them here (Tailwind rules do not apply to this folder).
- To update: re-copy the file with `git show <commit>:src/kit/<path>` from the kit commit you move to, bump that folder's commit above, and re-run `npm run build`, `npx vitest run` and the affected e2e specs.
- Per-file imports: `pro/command-menu.tsx` from `a11da89`, used by `components/CommandPalette.tsx` (⌘K).
