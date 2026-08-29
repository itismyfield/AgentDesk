# Kakao scheduled-message delivery

AgentDesk can deliver an opted-in scheduled message to Discord, up to five
Kakao friends, and the connected Kakao account's **My Chatroom**. Kakao support
is disabled by default and reads secrets only from process environment
variables; tokens are never accepted through the dashboard or serialized into
runtime configuration responses.

## Kakao application prerequisites

Create a Kakao Developers application and configure Kakao Login. Friend sends
also require Kakao's separate message-send permission. The user token must have:

- `talk_message` for My Chatroom and friend sends
- `friends` for selecting or resolving eligible friends

Kakao restricts friend delivery to users of the same service, permits at most
five recipients per request, and applies provider quotas. See the official
[Kakao Talk message REST API](https://developers.kakao.com/docs/ko/kakaotalk-message/rest-api)
and [Kakao Login token API](https://developers.kakao.com/docs/ko/kakaologin/rest-api).

An application REST API key alone cannot send a message. AgentDesk also needs
either a user refresh token (recommended) or a currently valid user access
token obtained after the required consent.

## Enable one account

Restart AgentDesk after setting the environment:

```bash
AGENTDESK_KAKAO_ENABLED=true
AGENTDESK_KAKAO_LANDING_URL=https://agentdesk.example.com/messages

KAKAO_REST_API_KEY=your-rest-api-key
KAKAO_CLIENT_SECRET=your-client-secret
KAKAO_REFRESH_TOKEN=the-user-refresh-token
```

`KAKAO_CLIENT_SECRET` is optional when the Kakao application does not require
one. `KAKAO_ACCESS_TOKEN` can be supplied as a short-lived fallback. When a
refresh token is present, AgentDesk refreshes once before the first send and
keeps rotated access/refresh tokens only in process memory.

## Enable multiple accounts

Account IDs use lowercase letters, digits, and hyphens. The `default` account
keeps the unqualified `KAKAO_*` names; other accounts use an uppercase prefix:

```bash
AGENTDESK_KAKAO_ACCOUNTS=default,work-bot
AGENTDESK_KAKAO_DEFAULT_ACCOUNT=default

KAKAO_WORK_BOT_REST_API_KEY=your-work-app-key
KAKAO_WORK_BOT_CLIENT_SECRET=your-work-client-secret
KAKAO_WORK_BOT_REFRESH_TOKEN=the-work-user-refresh-token
```

This explicit allowlist prevents a request from selecting an arbitrary
environment-variable prefix.

## Delivery safety

- Provider URLs are fixed in code; configuration cannot redirect tokens.
- HTTP redirects are disabled and requests have a 10-second timeout.
- Linked landing/image URLs must be public HTTPS URLs without credentials or a
  custom port.
- Message text is limited to 200 characters and friend UUIDs to five unique,
  printable identifiers.
- A transport failure or malformed success response is treated as an ambiguous
  outcome. The durable external-delivery outbox does not automatically replay
  an ambiguous Kakao send, avoiding duplicate messages.
- Logs and API errors report counts/status only, never tokens or friend UUIDs.

The scheduled-message request fields and examples are documented with the
scheduled fan-out PR that consumes this transport.
