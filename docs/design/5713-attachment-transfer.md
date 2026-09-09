# #5713 S1 — AttachmentBundleV1 byte contract

Multi-node intake blocks image/file messages whose channel belongs to another
node (`NonPortableAttachmentForeignOwner` / `NonPortableAttachmentRoutedTarget`)
because the leader has the bytes only on its own disk. S1 defines the envelope
that makes those bytes portable; it wires no runtime path.

## Envelope

`AttachmentBundleV1 { version, identity, entries }` in
`src/services/cluster/attachment_transfer.rs`.

- `identity` is `(provider, channel_id, user_msg_id)` — the one message the
  bytes belong to.
- `entries` is ordered; each is `(filename, byte_len, sha256, bytes)`.
- No CDN URL and no sending-node path. The receiver must materialize from the
  envelope alone, and an expired/re-signed URL must not turn a stored bundle
  into an unreadable one.
- `filename` is a display name. The receiver derives its storage name from the
  ordinal and digest (S2); the contract's filename check exists so the name is
  safe to *echo*, not safe to *join*.

## Atomicity and validation

`validate_attachment_bundle_v1` is all-or-nothing per message: unknown version,
identity mismatch, empty entries, count/per-entry/total ceilings, declared-vs-
actual length, or digest mismatch each reject the whole bundle. Only
`ValidatedAttachmentBundle` reaches a consumer, so a partially-checked bundle is
unrepresentable. Ceilings are injected by the caller; the shipped defaults are
10 entries / 8 MiB each / 16 MiB total. Every rejection is fail-closed — a
caller must never degrade an attachment turn to text-only.

## Out of scope, and rollback

Durable storage plus worker consumption (S2) and the live download/router
unblock (S3) own all I/O. Nothing in this slice is called from production, so
reverting it is a pure deletion: no migration, no config, no behaviour change.
