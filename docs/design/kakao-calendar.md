# Kakao Calendar v3 implementation review

Reviewed specification: `AgentDesk_Kakao_Calendar_Implementation_v3.md`, dated
2026-09-16. Code baseline: `4876806fe58f50d828a5c43e677ba40d2195d310`.
The implementation and operational contract are described in
[the integration runbook](../integrations/kakao-calendar.md).

## Scope and design decisions

An authenticated operator can create, list, read, revise and delete an
AgentDesk-managed timed, nonrecurring event across separately consenting accounts.
Each account receives its own primary-calendar event. This does not invite a
friend to a shared event. Mutation acceptance is durable PostgreSQL intent;
per-account application is asynchronous and visible separately from acceptance.

The implementation favors a small shared account runtime over a generic provider
framework. Messages and calendar share account parsing, one cached client,
serialized token refresh, generation-aware 401 recovery, bounded transport and
optional credential persistence. Message templates retain their own landing-URL
validation. Calendar-specific validation, API transport and durable synchronization
are separate modules. The secret-file atomic primitive has no Discord dependency.

Existing route registration, worker lifecycle, request fingerprinting, PostgreSQL
migrations, test fixtures and inventory generators are reused. Calendar intent
does not reuse the message outbox's delivery FK, recipient counts or terminal
payload lifetime. This is the boundary that permits calendar evolution without
changing message delivery semantics. Extensibility is supported by these seams;
there is no claim that a global maximum ROI has been measured.

Recovery policy and its closed set of resolutions live in the calendar service;
the HTTP handler only authenticates, extracts input and maps the result. The DB
read module shares one projection between detail and paginated list. One SQL
statement reads intent and target progress from the same PostgreSQL snapshot,
avoiding mixed revisions and the former per-event list round trips. Account
check timestamps are also fetched in one batch. Provider payload conversion
validates intent and returns an error instead of silently substituting an empty
timestamp. Recovery lease checks use the database clock that issued the lease.

## F01–F21 disposition

These are assessments of the pinned source, not claims of observed production incidents.

| Finding | Baseline assessment | Implementation / evidence |
| --- | --- | --- |
| F01 account ID mismatch | Confirmed | First character must be lowercase ASCII letter or digit; remaining characters match the existing DB rule. Parser regression tests retain digit-leading IDs. |
| F02 existing shared client cache | Already present | Moved from scheduled-message delivery to `kakao::account`; both consumers use it. No second cache introduced. |
| F03 refresh persistence | Missing | Opt-in Unix store, atomic replacement, process lock, persisted-token precedence, refresh omission preservation and save-failure blocking. Unix reopen test exercises rotation. |
| F04 landing URL coupling | Confirmed | Auth-only environment parsing supports calendar without a message URL; message send still validates its URL. |
| F05 POST-only transport | Confirmed | Shared authorization path supports fixed GET/POST/DELETE endpoints; feature adapters interpret their own success responses. Mock verifies forms and query strings. |
| F06 outbox is not calendar source | Existing contract, not a defect | Separate binding/event/target/request/operation tables; transactional acceptance and enqueue. |
| F07 sent versus arrival | Existing contract, not a defect | Message handoff preserved. Calendar returns 202 `accepted`, exposing applied revisions and aggregate outcomes separately. |
| F08 ready versus verified | Existing offline diagnostic | Offline connector probe avoids taking token-store ownership. Calendar account check performs live identity/consent checks; list never asserts current verification. |
| F09 secret helper limits | Confirmed capability gap | Added descriptor-relative, no-follow private-directory atomic writer; existing writers keep their contracts. |
| F10 HTTP dedupe TTL | Unsuitable for remote create safety | Calendar request keys persist independently of HTTP TTL and survive tombstones. Same-key concurrency and conflicting payloads are tested in PostgreSQL. |
| F11 claimed identity headers | Existing trust boundary | Every calendar handler requires the actual configured operator Bearer token. No local/no-token bypass; claimed headers do not authorize. Helper tests cover rejection. |
| F12 leader versus credential owner | Existing operational limitation | Calendar rejects cluster mode and requires one Unix credential owner with a locked private store. Multi-node failover is not claimed. |
| F13 missing transport coverage | Confirmed coverage gap | Added mock create/update/delete, identity/consent shape, parallel stale-401, transient refresh and bounded ambiguous-response tests. |
| F14 generated inventories | Applicable | Regenerated env, route, worker, architecture and test manifests using repository scripts. |
| F15 zero-success message summary | Confirmed branch defect | Classifies zero success as failure, true partial separately, malformed totals as unknown; unit regression test. |
| F16 message dispatch time fence | Confirmed missing checks | SQL now checks live lease and delivery deadline; PostgreSQL regression exercises each expiry and valid dispatch. |
| F17 stale 401 duplicate refresh | Confirmed missing generation check | Refresh compares the generation rejected by the provider under the account mutex; concurrent old-401 test observes one refresh. |
| F18 transient refresh classification | Confirmed overbroad reauthorization | Network/429/5xx and invalid success responses remain transient; credentials are retained. |
| F19 Discord atomic writer coupling | Existing unsuitable dependency | Private-file primitive lives under `utils::secret_file::private_directory`, without Discord telemetry or storage dependencies. Failure stages and temporary cleanup tested on Linux. |
| F20 recipient counts versus targets | Existing contract, not a defect | Calendar persists separate target outcomes and barriers. Mock + PostgreSQL test repairs only the failed account without recreating the successful one. |
| F21 partial external list objects | New adapter constraint | Managed list reads local intent; no arbitrary remote list is exposed or deserialized into required-ID objects. Recovery rejects insufficient detail evidence. |

## Verification and its limits

The targeted library suite uses a separate PostgreSQL 17 container and mock HTTP
servers. Coverage includes two-account CRUD through the real executor, partial
failure recovery, replay/conflict, transactional rollback, stale revision/lease,
identity uniqueness, unknown-create barriers, late success after lease expiry,
tombstone deletion and content scrubbing. Shared message regression coverage
includes its external outbox handoff and dispatch expiry.

Unix-only credential tests run additionally in a Rust 1.94 Linux container using
unchanged copies of the production modules in an isolated source harness. These
exercise file ownership/modes, symlinks, a second credential owner, write/sync/
rename/parent-sync failure stages, temporary cleanup and refresh-store reopen.
This harness is not a full Linux build of AgentDesk and does not use the entire
repository dependency lockfile. The same test sources are checked into the crate
for its Linux CI lanes.

Migration checksum, PostgreSQL test-lane membership, curated test-lane coverage,
library inventory and generated-document checks accompany the runtime tests.
Existing unrelated inventory/debt warnings are not hidden or broadly refactored.

Not established by these tests: real Kakao app approval and `talk_calendar`
consent, live writes, complete endpoint-level server/browser end-to-end coverage,
real process termination at every filesystem/HTTP interleaving, full-platform
repository suites, provider-side exactly-once behavior, or multi-node token
failover. No real user calendar or friend message was touched.

## Provider uncertainty and conservative behavior

External research of the official REST reference returned inconsistent synthesized
claims about five-minute event-time restrictions and some detail fields. This
does not justify restoring the restriction expressly withdrawn by v3. Input
times are not rounded or forced to five-minute boundaries; reminder offsets have
their separate validation. Live provider rejection remains visible as a rejected
operation. Location clearing and disabling reminders are rejected rather than
assigned an unverified remote meaning.

Recovery adoption requires a detail response with matching ID, primary calendar,
explicit `is_host:true`, title, time, description, location and explicitly supplied
reminders. Missing ownership/identity/time evidence fails closed. This is a
conservative evidence requirement, not a claim that every live detail response
will expose those fields. An operator must also verify that an uncertain created
event came from this service; host status alone does not establish creator-app
provenance. Existing remote mappings cannot be replaced during update recovery.
No broad remote-calendar search or automatic guessing is performed.

Unknown writes remain barriers. Explicit operator recovery requires stopping the
old credential owner and documenting evidence. A lease timeout, ambiguous 404 or
empty result is not proof that a remote create/delete did not occur. Known remote
events are never silently recreated when a detail or update fails. See the
runbook for credential setup, recovery and rollback.
