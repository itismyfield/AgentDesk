# Health Port Unification Checklist

AgentDesk prompt/skill/memory assets should follow these rules after the `/api/send` and `/api/senddm` routes were folded into the main axum API:

- [x] Do not hardcode port `8798` in prompts, skills, docs, or memory notes.
- [x] Use the active `server.port` value for local API calls such as `http://127.0.0.1:<port>/api/send`.
- [x] Do not reference `AGENTDESK_HEALTH_PORT`; the separate health listener no longer exists.
- [x] Use `credential/announce_bot_token` and `credential/notify_bot_token` as the bot-token source for agent-to-agent routing.
- [x] Treat `/api/health`, `/api/send`, and `/api/senddm` as endpoints on the same axum server.

## Verification (2026-03-23)

- `/api/health` on 8791: 200 OK, healthy
- `/api/send` on 8791: `{"ok": true}` (channel:1479671298497183835)
- `/api/senddm` on 8791: endpoint responds correctly (Discord-level DM restriction)
- `AGENTDESK_HEALTH_PORT` removed from both LaunchAgent plists
- Bot tokens: all code paths use `credential::read_bot_token()`, no yaml token dependency
