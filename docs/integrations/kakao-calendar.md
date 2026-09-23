# Kakao managed calendars

AgentDesk accepts a timed, nonrecurring event once and applies it independently to
each consenting Kakao account. A meeting for `default` and `friend` creates two
separate events. This is not a shared calendar, invitation, attendance response,
KakaoTalk inbound bot, or general personal-calendar reader.

## Enable and connect

The initial credential boundary is **one Unix AgentDesk process, cluster disabled**.
Windows native durable storage and multi-node credential failover fail closed.
The existing message-only feature continues to work without opting into storage.

1. In the Kakao app console, verify Kakao Login, applicable app membership/service
   permissions and the `talk_calendar` consent item. Each person must independently
   authorize the same app. A friend's message UUID does not grant calendar access.
   App review, personal-developer eligibility and business requirements are external
   console conditions; this implementation neither grants nor bypasses them.
2. Obtain each account's tokens through its authorized login flow. Supply bootstrap
   credentials privately to the service environment; do not put them in prompts,
   API request JSON, URLs, source control or recovery notes.
3. Configure:

   ```text
   AGENTDESK_KAKAO_CALENDAR_ENABLED=true
   AGENTDESK_KAKAO_ACCOUNTS=default,friend
   AGENTDESK_KAKAO_CALENDAR_ACCOUNTS=default,friend
   AGENTDESK_KAKAO_TOKEN_STORE_DIR=<absolute private directory owned by the service user>
   KAKAO_REST_API_KEY=<app key>
   KAKAO_REFRESH_TOKEN=<default account bootstrap refresh token>
   KAKAO_FRIEND_REST_API_KEY=<same app key>
   KAKAO_FRIEND_REFRESH_TOKEN=<friend account bootstrap refresh token>
   ```

   Configure `KAKAO_CLIENT_SECRET` / `KAKAO_FRIEND_CLIENT_SECRET` if enabled in the
   app console. Existing `KAKAO_ACCESS_TOKEN` and account-prefixed equivalents are
   supported bootstrap inputs, but renewable tokens are needed for unattended use.
   Account names match `^[a-z0-9][a-z0-9-]{0,31}$`; `work-bot` maps to `KAKAO_WORK_BOT_*`.
4. Pre-create the token-store directory as the AgentDesk Unix user, mode `0700`,
   with no symlink components. Files use `0600`; locks prevent a second runtime
   from owning the same account store. Storage is permissions-restricted, **not
   encrypted** and not isolated from another process running as that OS user.
5. Set a nonempty AgentDesk `server.auth_token`. Every calendar endpoint requires
   its actual `Authorization: Bearer ...` value, including localhost reads and
   idempotent replay. Claimed agent/channel headers and same-origin bypass do not
   authorize calendar access. Agents sharing this token share operator authority.
6. Restart and call `POST /api/kakao/calendar/accounts/default/check`, then the
   `friend` equivalent. Checks verify app/user identity and current consent, reject
   duplicate aliases for one identity, and bind aliases immutably in PostgreSQL.

Calendar-only use does not require `AGENTDESK_KAKAO_ENABLED` or a message landing
URL. Message delivery retains those requirements. Both features use the same
account runtime and refresh generation. Status `configured` is an offline setting
check; only an explicit check reports current verified identity/consent.

Stored tokens take precedence over bootstrap environment tokens. Changes to
credentials require a controlled restart. To reconnect the **same** identity,
stop the credential owner, privately replace/remove that account's stored token
file and provide fresh bootstrap credentials, then restart and check. Never
delete a live lock file. A different person must use a new alias: remove the old
alias from the calendar allowlist, retain its outstanding evidence, and configure
and check the new alias. Old event targets are never silently rebound.

## Managed event API

Use `/api/help` and `/api/docs/integrations/kakao-calendar` for endpoint discovery.
All event IDs in ordinary CRUD are internal UUIDs, not Kakao event IDs.

```http
POST /api/kakao/calendar/events
Authorization: Bearer <AgentDesk operator token>
Idempotency-Key: <stable originating action ID>
Content-Type: application/json

{
  "accountIds": ["default", "friend"],
  "title": "Joint meeting",
  "time": {
    "startAt": "2026-09-30T10:00:00+09:00",
    "endAt": "2026-09-30T11:00:00+09:00",
    "timeZone": "Asia/Seoul"
  },
  "description": "Agenda",
  "location": {"name": "Meeting room"},
  "reminders": [60]
}
```

`202 {eventId, revision, status:"accepted"}` means the intent and target jobs were
committed atomically. Poll `GET /api/kakao/calendar/events/{eventId}` for each
account's state and applied revision. `success` requires every target to apply;
`partial_success`, `failed`, `unknown` and `accepted` distinguish other outcomes.
An older uncertain operation remains visible even after a newer revision is queued.

Keep the originating action key in the agent's task/tool history and forward it
unchanged when an agent retries or falls back. The API rejects a missing key;
requests are retained without the generic HTTP cache's 24-hour expiry. Same key,
different normalized request is a conflict. Target ordering is normalized.
The server cannot infer that two unrelated fresh keys mean the same user action.

Update with a new action key, reusing that key only for retries of this update:

```http
PATCH /api/kakao/calendar/events/<internal UUID>
Idempotency-Key: <stable update action ID>
Authorization: Bearer <AgentDesk operator token>
Content-Type: application/json

{"expectedRevision":1,"title":"Updated meeting","description":null}
```

Omission keeps an optional field, a value sets it, and `description:null` clears it.
Required-field null and unsupported clearing requests are rejected. Location
clearing is not inferred from undocumented provider behavior. Empty reminders are
not treated as disabling notifications. Nonempty reminders support up to two
distinct five-minute offsets, 0–43,200 minutes. There is no silent time rounding.
RFC3339 offsets must agree with the IANA zone; provider timestamps are normalized
to UTC while the zone is preserved. Unsupported recurring/all-day fields and
unknown input fields are rejected.

Delete uses the same headers and JSON `{"expectedRevision":2}` with a new action
key on `DELETE /api/kakao/calendar/events/{eventId}`. A persistent tombstone prevents
old undispatched work from resurrecting the event. Once all targets are deleted,
event details and job snapshots are scrubbed; request fingerprints, IDs, revisions,
binding and outcome metadata remain for deduplication. Active and unresolved
events retain the content necessary for execution/recovery. Retention of these
records follows the operator's resolution of pending work; they are not TTL-expired
into new remote side effects.

List accepts `limit` (1–100, default 25) and `before` from the last `nextCursor`.
Only managed events whose targets are all currently allowed are returned.
`GET .../{eventId}/operations` returns up to 200 recent content-free operation
records. Mutation bodies are bounded to 32 KiB; recovery bodies to 8 KiB.

## Failure and recovery

- `queued`: accepted, no remote dispatch yet.
- `preparing`: exclusive claim; identity/consent are checked before the final fence.
- `dispatching`: remote call may have started. Lease expiry becomes uncertainty.
- `blocked`: credential, consent, binding or configuration needs attention.
- `rejected`: a definite rejection; inspect the redacted error code.
- `needs_reconcile`: the write may have applied. Do not recreate automatically.
- `applied`: confirmed response persisted with the remote ID and applied revision.
- `superseded`: a newer intent replaced undispatched work.

Claims serialize each target. An uncertain earlier write blocks that target's
later revisions, including deletion, without recreating a successful other target.
The fence checks token, lease, revision and deletion intent. It does not cancel an
HTTP request already at Kakao or promise provider-side exactly-once execution.
Late confirmed responses with the original claim token can still be recorded.

Use `POST .../{eventId}/operations/{operationId}/recover` with operator Bearer
authentication. Include a 10–1000 byte non-secret `note`, `resolution` and
`credentialOwnerRestarted`. Check the original account binding first.

- `retry`: after fixing a blocked/definitely rejected operation. Unknown writes
  cannot use this path. Successful targets are not recreated.
- `adopt`: for a known remote ID after uncertainty, additionally provide
  `remoteEventId`. The service verifies it through the original account's detail
  endpoint and requires matching calendar/ownership/content evidence; ambiguous
  or unsupported responses are rejected. The operator must establish that the
  event was created by this service; host status alone is not creator-app proof.
  Existing remote mappings cannot be replaced when recovering an update.
- `confirm_not_applied`: explicit operator judgment after resolving whether the
  call applied. Stop the old credential owner and investigate the remote calendar
  before setting `credentialOwnerRestarted:true`. A lease timeout or empty list
  is not proof that creation failed. Record the actual evidence in `note`.

The recovery attestation is an operator decision, not an additional authorization
factor or a claim of remote exactly-once safety. An unresolved delete or an event
manually removed outside AgentDesk is not silently treated as successful based on
an ambiguous 404/authorization error. Verify the documented provider response and
use the recovery procedure rather than automatic recreation.

File persistence errors retain the newly rotated token in memory and block further
use until a save succeeds. A parent-sync error can mean the new file is already
visible; do not restore stale tokens. Logs identify only the failed storage stage.
For repeated pre-dispatch transport failures, backoff is bounded to five attempts;
subsequent recovery is explicit.

## Disable and rollback

Set `AGENTDESK_KAKAO_CALENDAR_ENABLED=false` and restart. This stops new acceptance
and dispatch; it does not delete remote events. Keep migrations 0127/0128 and its data
when rolling back the binary. Preserve token files, unknown jobs and dedupe evidence.
Remove a specific alias from the calendar allowlist to revoke its calendar access.
Messages remain independently controlled by their existing enable flag.

Live account writes, actual friend messages, app review and deployment require
separate operator authorization. Tests in this change use mock HTTP and isolated
PostgreSQL, not real Kakao user calendars.

## Provider references

Contracts are sourced from the [Kakao Talk Calendar REST API](https://developers.kakao.com/docs/ko/talkcalendar/rest-api),
[Talk Calendar overview](https://developers.kakao.com/docs/ko/talkcalendar/common),
and [Kakao Login REST API](https://developers.kakao.com/docs/ko/kakaologin/rest-api).
The reviewed v3 specification was dated 2026-09-16 and pinned AgentDesk
`4876806fe58f50d828a5c43e677ba40d2195d310`. Mock tests establish our transport and
state-machine behavior; they are not evidence of console approval or live-account
availability.
