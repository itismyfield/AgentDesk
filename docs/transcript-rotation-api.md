# Transcript rotation handoff API

The runtime session API exposes these endpoints when
`runtime.transcript_rotation_soft_bytes` is set at server startup. Unset, both
endpoints return `404`. Use the session's current key or a registered alias
and send the request to the node that owns the session.

## Submit a handoff

`POST /api/sessions/{key}/rotation-handoff` takes an application/json object.

```json
{
  "op_id": "68e54b09-c47a-46f5-be89-d3b075c84a1f",
  "native_session_id": "6dd4d3f0-613c-4a4b-bdd7-ede288bdfde0",
  "ticket": "6a213752-b5e7-43ff-88f2-47acd54d82fa",
  "body": "Continue from the verified checkpoint."
}
```

The ticket authorizes one body for its request. A successful response is
`200` with `op_id`, the hex SHA-256 `digest` of the exact UTF-8 body, and
the original `stored_at` timestamp. An identical retry returns that same
receipt even after the ticket closes. A different body or request identity
returns `409`. A matching closed ticket without a stored receipt returns
`410`. When the mailbox has yet to recover, a first submission returns
`503` and can be retried. The endpoint durably stores the text and receipt.

## Read handoff status

`GET /api/sessions/{key}/rotation-handoff` returns `{"rotation": null}`
for a known session without a request. Otherwise, `rotation` contains
`op_id`, `native_session_id`, `requested_at`, `expires_at`, `state`
(`requested`, `staged`, or `closed`), and the saved `body` and `receipt`
when available. The API omits ticket material and sends
`Cache-Control: no-store` with successful responses.
