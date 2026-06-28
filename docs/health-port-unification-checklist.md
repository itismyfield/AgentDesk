# Health Port Unification Checklist

AgentDesk prompt/skill/memory assets should follow these rules after the `/api/discord/send` and `/api/discord/send-dm` routes were folded into the main axum API:

- [x] Do not hardcode port `8798` in prompts, skills, docs, or memory notes.
- [x] Use the active `server.port` value for local API calls such as `http://127.0.0.1:<port>/api/discord/send`.
- [x] Do not reference `AGENTDESK_HEALTH_PORT`; the separate health listener no longer exists.
- [x] Use `credential/announce_bot_token` and `credential/notify_bot_token` as the bot-token source for agent-to-agent routing.
- [x] Use `POST /api/discord/bot-tokens/reload` after rotating `credential/announce_bot_token` or `credential/notify_bot_token`; each bot response has a `status` such as `reloaded` or `missing_or_invalid` plus a `previous_client_kept` boolean without exposing token material. The response includes `report.scopes.utility_rest_clients.restart_required=false`.
- [x] Treat provider runtime bot tokens as restart-scoped: `SharedData.cached_bot_token` is a `OnceCell`, so this reload endpoint does not rotate gateway/provider bots until dcserver restarts. Check `report.scopes.provider_runtime_cached_token.restart_required=true`, `report.scopes.provider_gateway_session.restart_required=true`, or `/api/health/detail` `bot_token_reload_scopes.*.restart_required`.
- [x] Treat `/api/health`, `/api/discord/send`, and `/api/discord/send-dm` as endpoints on the same axum server.

## Bot Token Rotation Procedure

Utility bot rotation (`announce` / `notify`):

1. Write the new token to `credential/announce_bot_token` and/or `credential/notify_bot_token` in the active AgentDesk runtime root.
2. Reload the utility REST clients from the dcserver host:

   ```bash
   curl -fsS -X POST http://127.0.0.1:8791/api/discord/bot-tokens/reload | jq .
   ```

   For a non-loopback caller, include the configured API bearer token:

   ```bash
   curl -fsS -X POST http://<host>:8791/api/discord/bot-tokens/reload \
     -H "Authorization: Bearer $AGENTDESK_API_TOKEN" | jq .
   ```

3. Confirm `report.announce.status` and/or `report.notify.status` is `reloaded`. If a rotated utility credential reports `missing_or_invalid`, fix the file and call the endpoint again. Do not paste token values into logs or issue comments.

Provider runtime / gateway bot rotation:

1. Update the provider bot token source: `discord.bots.<name>.token` in `config/agentdesk.yaml` or `credential/<name>_bot_token`.
2. Call the reload endpoint only to refresh utility clients and to confirm the runtime scope status. It will continue to report `provider_runtime_cached_token.restart_required=true` and `provider_gateway_session.restart_required=true`.
3. Restart dcserver after active turns have drained:

   ```bash
   agentdesk restart-dcserver
   ```

4. Verify the post-restart diagnostic surface:

   ```bash
   curl -fsS http://127.0.0.1:8791/api/health/detail | jq '.bot_token_reload_scopes'
   ```

   The provider runtime and gateway scopes still report `restart_required=true` as a capability statement: future provider-token rotations require another dcserver restart.

## Verification (2026-03-23)

- `/api/health` on 8791: 200 OK, healthy
- `/api/discord/send` on 8791: `{"ok": true}` (channel:1479671298497183835)
- `/api/discord/send-dm` on 8791: endpoint responds correctly (Discord-level DM restriction)
- `AGENTDESK_HEALTH_PORT` removed from both LaunchAgent plists
- Bot tokens: all code paths use `credential::read_bot_token()`, no yaml token dependency
