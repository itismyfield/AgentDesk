# Health Port Unification Checklist

AgentDesk prompt/skill/memory assets should follow these rules after the `/api/discord/send` and `/api/discord/send-dm` routes were folded into the main axum API:

- [x] Do not hardcode port `8798` in prompts, skills, docs, or memory notes.
- [x] Use the active `server.port` value for local API calls such as `http://127.0.0.1:<port>/api/discord/send`.
- [x] Do not reference `AGENTDESK_HEALTH_PORT`; the separate health listener no longer exists.
- [x] Use `credential/announce_bot_token` and `credential/notify_bot_token` as the bot-token source for agent-to-agent routing.
- [x] Use `POST /api/discord/bot-tokens/reload` after rotating `credential/announce_bot_token` or `credential/notify_bot_token`; each bot response has a `status` such as `reloaded` or `missing_or_invalid` plus a `previous_client_kept` boolean without exposing token material.
- [x] Treat provider runtime bot tokens as restart-scoped: `SharedData.cached_bot_token` is a `OnceCell`, so this reload endpoint does not rotate gateway/provider bots until dcserver restarts.
- [x] Treat `/api/health`, `/api/discord/send`, and `/api/discord/send-dm` as endpoints on the same axum server.

## Verification (2026-03-23)

- `/api/health` on 8791: 200 OK, healthy
- `/api/discord/send` on 8791: `{"ok": true}` (channel:1479671298497183835)
- `/api/discord/send-dm` on 8791: endpoint responds correctly (Discord-level DM restriction)
- `AGENTDESK_HEALTH_PORT` removed from both LaunchAgent plists
- Bot tokens: all code paths use `credential::read_bot_token()`, no yaml token dependency
