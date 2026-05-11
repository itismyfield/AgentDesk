# Voice Channel Migration Runbook

This runbook covers the migration from per-agent Discord text channels to per-agent `GUILD_VOICE` channels with embedded text chat.

Discord cannot convert an existing text channel into a voice channel in place. The safe migration pattern is:

1. Create a new voice channel with the same name and category as the old text channel.
2. Replace ADK channel bindings in `~/.adk/release/config/agentdesk.yaml`.
3. Replace the DB materialization in `agents.discord_channel_*`.
4. Archive the old text channel by renaming it to `<name>-archive`, moving it to the archive category, and making it readonly.
5. Keep the old/new channel-id mapping below for rollback.

## Target Channels

Snapshot source: `~/.adk/release/config/agentdesk.yaml`, captured 2026-05-11 KST.

`new_voice_channel_id` stays `TBD` until `scripts/voice-channel-migrate.sh --apply` creates or receives the new voice channel id.

| group | agent | provider(s) | old_text_channel_id | old_channel_name | new_voice_channel_id | status |
| --- | --- | --- | --- | --- | --- | --- |
| CookingHeart | `ch-td` | `claude` | `1475086789696946196` | `cookingheart-dev-cc` | TBD | planned |
| CookingHeart | `ch-td` | `codex` | `1479430394431668336` | `cookingheart-dev-cdx` | TBD | planned |
| CookingHeart | `ch-dd` | `claude` | `1474933804887179286` | `cookingheart-design-cc` | TBD | planned |
| CookingHeart | `ch-dd` | `codex` | `1479430392917262406` | `cookingheart-design-cdx` | TBD | planned |
| CookingHeart | `ch-pd` | `claude` | `1478652616295583805` | `cookingheart-pd-cc` | TBD | planned |
| CookingHeart | `ch-pd` | `codex` | `1479430391365505114` | `cookingheart-pd-cdx` | TBD | planned |
| CookingHeart | `ch-tad` | `claude` | `1478652735300829185` | `cookingheart-tad-cc` | TBD | planned |
| CookingHeart | `ch-tad` | `codex` | `1479430396214251520` | `cookingheart-tad-cdx` | TBD | planned |
| CookingHeart | `ch-ad` | `claude` | `1478653028000338114` | `cookingheart-ad-cc` | TBD | planned |
| CookingHeart | `ch-ad` | `codex` | `1479430397975728169` | `cookingheart-ad-cdx` | TBD | planned |
| CookingHeart | `ch-qad` | `claude` | `1478653071214252103` | `cookingheart-qad-cc` | TBD | planned |
| CookingHeart | `ch-qad` | `codex` | `1479430399472959568` | `cookingheart-test-cdx` | TBD | planned |
| CookingHeart | `ch-pmd` | `claude` | `1478652416533463101` | `cookingheart-notice` | TBD | planned |
| CookingHeart | `ch-pmd` | `codex` | `1479430388353994775` | `cookingheart-pm-cdx` | TBD | planned |
| AgentDesk | `project-agentdesk` | `claude` | `1479671298497183835` | `adk-cc` | TBD | planned |
| AgentDesk | `project-agentdesk` | `codex` | `1479671301387059200` | `adk-cdx` | TBD | planned |
| AgentDesk | `adk-dashboard` | `claude` | `1490141479707086938` | `adk-dash-cc` | TBD | planned |
| AgentDesk | `adk-dashboard` | `codex` | `1490141485167808532` | `adk-dash-cdx` | TBD | planned |
| AgentDesk | `project-agentmanager` | `claude` | `1480015244062490774` | `agent-factory` | TBD | planned |
| AgentDesk | `project-skillmanager` | `claude` | `1484175866194821231` | `skill-manager` | TBD | planned |
| AgentDesk | `project-scheduler` | `claude` | `1480029681205121200` | `scheduler` | TBD | planned |
| AgentDesk | `adk-deadlock-manager` | `claude` | `1484912492202168431` | `deadlock-manager` | TBD | planned |
| Personal | `personal-obiseo` | `claude,codex` | `1478248459445469264` | `원창-cc` | TBD | planned |
| Personal | `personal-yobiseo` | `claude,codex` | `1478248518950060183` | `지연-cc` | TBD | planned |
| Family | `family-counsel` | `claude` | `1473922824350601297` | `윤호네비서` | TBD | planned |
| Family | `family-routine` | `codex` | `1473922780322988132` | `쇼핑도우미` | TBD | planned |
| Family | `chef-goat` | `claude` | `1480531508820185241` | `최고의셰프` | TBD | planned |

Initial exclusions:

| agent | channel | reason |
| --- | --- | --- |
| `project-newsbot` | `ai-news` | broadcast/news channel; voice turn-taking is not useful by default |
| `token-manager` | `token-manager` | operational reporting channel; migrate only after conversational agents are stable |
| `adk-cdx` | `agentdesk-cdx` / `1001` | local/test binding, not a live Discord snowflake |

## Dry Run

Run the full plan:

```bash
scripts/voice-channel-migrate.sh --dry-run
```

Run a single target plan:

```bash
scripts/voice-channel-migrate.sh --dry-run --agent project-agentdesk --provider codex
```

The dry run prints:

- target agent/provider rows from `agentdesk.yaml`
- old channel ids and DB columns that would change
- whether a new voice channel would be created or an existing `--new-channel` reused
- hardcoded old-channel-id hits under release agent prompts, release memories, and release skills

After a real apply, rerun the dry run for the old channel id. The hardcoded-id section should print `(none)` for agent memory/skill roots before the old text channel is archived permanently.

## Apply

Apply one channel at a time. Do not run broad apply without `--old-channel`.

Create a new voice channel through Discord REST and migrate bindings:

```bash
DISCORD_BOT_TOKEN="$DISCORD_BOT_TOKEN" \
VOICE_ARCHIVE_CATEGORY_ID="<archive-category-id>" \
scripts/voice-channel-migrate.sh \
  --apply \
  --old-channel 1490141485167808532 \
  --confirm
```

Reuse a manually created voice channel:

```bash
scripts/voice-channel-migrate.sh \
  --apply \
  --old-channel 1490141485167808532 \
  --new-channel "<new-voice-channel-id>" \
  --skip-discord \
  --confirm
```

`--apply` updates:

- `~/.adk/release/config/agentdesk.yaml`, with a timestamped backup next to it
- legacy `~/.adk/release/config/role_map.json` if present and if it contains the old channel id
- release agent prompts, memories, and skills that contain the old channel id, with timestamped backups
- `agents.discord_channel_id`, `agents.discord_channel_alt`, `agents.discord_channel_cc`, and `agents.discord_channel_cdx`
- the old Discord text channel archive state, unless `--skip-archive` is passed

For Postgres DB updates, the script uses `DATABASE_URL` when set. Otherwise it reads `database.host`, `database.port`, `database.dbname`, and `database.user` from `agentdesk.yaml` and calls `psql`.

## Pilot Verification

Recommended first pilot: `adk-dashboard` / `codex` (`1490141485167808532`) because it is lower-risk than active implementation channels.

Verification sequence:

1. Run `scripts/voice-channel-migrate.sh --dry-run --old-channel <old-id>`.
2. Run apply for exactly one old channel id.
3. Restart the release control plane if the running process does not hot-reload config.
4. Send a normal text message in the new voice channel embedded text chat and verify the expected agent responds.
5. Join the voice channel and verify voice bridge join/leave and TTS routing still point at the new channel.
6. Confirm in the Discord client that the old text channel is named `<name>-archive`, is readonly, and is under the archive category.
7. Run `agentdesk config audit --dry-run` and confirm no YAML/DB drift remains.

## Rollback

Rollback is config/DB-first. It does not delete the new voice channel, so Discord state can be inspected before cleanup.

Dry-run rollback:

```bash
scripts/voice-channel-migrate.sh \
  --rollback \
  --dry-run \
  --old-channel "<old-text-channel-id>" \
  --new-channel "<new-voice-channel-id>"
```

Apply rollback:

```bash
scripts/voice-channel-migrate.sh \
  --rollback \
  --apply \
  --old-channel "<old-text-channel-id>" \
  --new-channel "<new-voice-channel-id>" \
  --confirm
```

After rollback:

1. Move the old text channel out of the archive category.
2. Restore send permissions for the roles/users that should write there.
3. Restart the release control plane if needed.
4. Send a text message to the old channel and confirm the agent responds there.
5. Delete or archive the unused new voice channel only after verification.

## Mapping Log

Append actual migrations here as they happen.

| applied_at_kst | agent | provider(s) | old_text_channel_id | new_voice_channel_id | operator | verification |
| --- | --- | --- | --- | --- | --- | --- |
| TBD | TBD | TBD | TBD | TBD | TBD | TBD |
