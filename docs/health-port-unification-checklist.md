# Health Port Unification Checklist

AgentDesk prompt/skill/memory assets should follow these rules after the `/api/send` and `/api/senddm` routes were folded into the main axum API:

- [ ] Do not hardcode port `8798` in prompts, skills, docs, or memory notes.
- [ ] Use the active `server.port` value for local API calls such as `http://127.0.0.1:<port>/api/send`.
- [ ] Do not reference `AGENTDESK_HEALTH_PORT`; the separate health listener no longer exists.
- [ ] Use `credential/announce_bot_token` and `credential/notify_bot_token` as the bot-token source for agent-to-agent routing.
- [ ] Treat `/api/health`, `/api/send`, and `/api/senddm` as endpoints on the same axum server.
