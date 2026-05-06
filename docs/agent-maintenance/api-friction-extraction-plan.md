# API Friction Extraction Plan

> Last refreshed: 2026-05-06 (against #1789 extraction-planning pass).

Source issue: #1789
Epic: #1788

`src/services/api_friction.rs` is currently 1,709 lines: about 1,090 lines of
production marker capture, PG persistence, memory sync, pattern aggregation,
GitHub issue creation, and issue-body rendering followed by about 615 lines of
inline tests behind `#[cfg(all(test, feature = "legacy-sqlite-tests"))]`.

## Scope Notes

- Do not start behavior extraction in #1789. This document plus the linked
  follow-up issues are the deliverable.
- Keep the public `crate::services::api_friction::*` API stable until a later
  cleanup issue deliberately narrows call sites.
- Preserve the exact `API_FRICTION: {...}` marker contract. Valid markers are
  removed from user-visible text, invalid markers remain visible with parse
  errors, and field aliases/truncation stay unchanged.
- Keep runtime writes PG-only. `record_api_friction_reports` may accept a
  legacy DB handle for older call signatures, but event persistence must not
  add a SQLite fallback.
- Keep GitHub issue creation, `api_friction_issues` upserts, and memory status
  writes as best-effort side effects with the current failure reporting.
- `src/services/api_friction.rs` should end as a small facade/re-export module,
  not as the long-term home for parser, repository, reporting, or processor
  behavior.

## Current Consumers

| Consumer | API surface |
| --- | --- |
| `src/services/discord/turn_bridge/mod.rs` | Extracts markers from provider responses and records captured reports with dispatch/session context. |
| `src/server/mod.rs` | Runs repeated-pattern processing from the background scheduler. |
| Inline tests in `src/services/api_friction.rs` | Cover parsing, PG aggregation, Memento sync, PG-only writes, GitHub issue creation, and bigint issue context. |

## Subdomain Map

| Proposed module | Current source ranges | Approx. prod LOC | Approx. test LOC | Responsibility | Main consumers |
| --- | --- | ---: | ---: | --- | --- |
| `api_friction::mod` facade plus `types` | `src/services/api_friction.rs:1` through `src/services/api_friction.rs:157` | 120 retained | 0 | Module declarations, stable re-exports, constants, and public DTOs shared by parser, capture, aggregation, and processing. | `turn_bridge`, `server::mod`, owner modules. |
| `api_friction::test_support` | `src/services/api_friction.rs:1100` through `src/services/api_friction.rs:1247` | 0 | 150 | PG test lifecycle, mock `gh` issue creation, env restoration, and local Memento HTTP response helpers. | Per-owner API friction tests. |
| `api_friction::parser` | `src/services/api_friction.rs:31`, `src/services/api_friction.rs:158` through `src/services/api_friction.rs:188`, `src/services/api_friction.rs:415` through `src/services/api_friction.rs:533` | 190 | 35 | Raw marker DTO, marker extraction, report validation, response cleanup, field normalization, keyword collection, and truncation. | `turn_bridge`, parser tests. |
| `api_friction::source_context` | `src/services/api_friction.rs:124`, `src/services/api_friction.rs:544` through `src/services/api_friction.rs:630` | 95 | 55 | Dispatch/session source lookup, repo/card/issue/task/agent attribution, and bigint GitHub issue-number handling. | Event storage, Memento request builder, evidence tests. |
| `api_friction::event_store` | `src/services/api_friction.rs:139`, `src/services/api_friction.rs:631` through `src/services/api_friction.rs:733`, `src/services/api_friction.rs:797` through `src/services/api_friction.rs:828` | 145 | 90 | Event row preparation, fingerprint generation, JSON payload/keyword serialization, and transactional PG insertion. | Capture flow, pattern aggregation, PG-only write tests. |
| `api_friction::memory_sync` | `src/services/api_friction.rs:133`, `src/services/api_friction.rs:189` through `src/services/api_friction.rs:263`, `src/services/api_friction.rs:534` through `src/services/api_friction.rs:543`, `src/services/api_friction.rs:734` through `src/services/api_friction.rs:796`, `src/services/api_friction.rs:829` through `src/services/api_friction.rs:849` | 210 | 210 | Public record orchestration, Memento backend resolution, Memento request construction, token usage accumulation, and per-event memory status updates. | `turn_bridge`, Memento sync tests. |
| `api_friction::patterns` | `src/services/api_friction.rs:77`, `src/services/api_friction.rs:850` through `src/services/api_friction.rs:965` | 130 | 65 | Repeated-pattern aggregation, minimum-count policy, API limit clamping, latest event row mapping, and existing issue metadata loading. | Processor, scheduler, aggregation tests. |
| `api_friction::issue_body` | `src/services/api_friction.rs:966` through `src/services/api_friction.rs:1091` | 130 | 50 | Evidence lookup, evidence DTO, issue body rendering, fallback wording, and evidence cap. | GitHub issue processor, reporting tests. |
| `api_friction::processor` | `src/services/api_friction.rs:96` through `src/services/api_friction.rs:122`, `src/services/api_friction.rs:264` through `src/services/api_friction.rs:414` | 180 | 80 | Public repeated-pattern processor, existing-issue skip policy, GitHub issue creation, `api_friction_issues` success/failure upserts, and process summary DTOs. | `server::mod`, scheduler, GitHub issue tests. |
| Owner test modules | `src/services/api_friction.rs:1248` through `src/services/api_friction.rs:1709` | 0 | 460 | Parser, pattern aggregation, Memento sync, PG-only write, GitHub issue creation, and source-context bigint tests moved beside owners. | Extracted owner modules. |

## Recommended Extraction Order

1. #1830 `api_friction: create facade module shell and shared test support`
   - Required first because Rust cannot keep `src/services/api_friction.rs`
     and `src/services/api_friction/` as the same module.
   - Acceptance focus: stable public API, no runtime behavior changes, shared
     test helpers available for later owner modules.

2. #1831 `api_friction: extract marker parsing and normalization`
   - Lowest-risk production slice because it is pure text/JSON validation.
   - Acceptance focus: valid marker stripping, invalid marker preservation, and
     field alias/truncation behavior.

3. #1832 `api_friction: extract source context and event storage`
   - Establishes the repository boundary before memory and aggregation modules
     depend on persisted events.
   - Acceptance focus: PG-only event insertion, dispatch/session attribution,
     fingerprint stability, and bigint issue-number handling.

4. #1833 `api_friction: extract memory sync handling`
   - Keeps Memento side effects separate from core event persistence.
   - Acceptance focus: file backend skip behavior, token usage accumulation,
     and best-effort memory status writes.

5. #1834 `api_friction: extract pattern aggregation queries`
   - Pulls repeated-pattern reads into a read-focused owner before GitHub issue
     processing moves.
   - Acceptance focus: minimum repeat count, limit clamping, latest-row
     projection, and existing issue metadata.

6. #1835 `api_friction: extract issue body evidence reporting`
   - Pure formatting plus bounded evidence lookup; should stay independent of
     GitHub issue creation.
   - Acceptance focus: rendered sections and evidence formatting remain stable.

7. #1836 `api_friction: extract GitHub issue processing`
   - Highest side-effect slice because it creates GitHub issues and upserts
     `api_friction_issues`.
   - Acceptance focus: existing issue skip behavior, success upsert, failure
     upsert, and process summary contents.

8. #1837 `api_friction: relocate inline tests by owner`
   - Final drain of the large feature-gated inline test block after owner
     modules and shared fixtures exist.
   - Acceptance focus: facade keeps only module wiring and optional API smoke
     tests.

## Test Migration Map

| Test area | Move with |
| --- | --- |
| Valid marker stripping, invalid marker preservation, alias handling, field cleanup | `api_friction::parser`. |
| Dispatch/session source context and bigint GitHub issue numbers | `api_friction::source_context`. |
| PG-only event insertion and SQLite handle non-use | `api_friction::event_store` plus `api_friction::memory_sync` for memory status assertions. |
| Memento request HTTP flow, token usage, backend skip, memory error/status writes | `api_friction::memory_sync`. |
| Repeated row counting and latest event summary projection | `api_friction::patterns`. |
| Issue body rendering and bounded evidence rows | `api_friction::issue_body`. |
| GitHub issue creation once, existing issue skip, success/failure upsert | `api_friction::processor`. |
| PG lifecycle, mock `gh`, env restoration, local HTTP response server | `api_friction::test_support`. |

## Dependency Rules

- `parser` should stay DB-free and memory-free.
- `event_store` may depend on `source_context` and shared `types`; it should
  not call Memento or GitHub.
- `memory_sync` may call `event_store` and update memory status, but it should
  not own pattern aggregation or issue creation.
- `patterns` should remain read-only and should not depend on GitHub issue
  creation.
- `issue_body` may read bounded evidence rows and format Markdown; it should
  not mutate `api_friction_issues`.
- `processor` may call `patterns`, `issue_body`, and GitHub issue creation, and
  owns `api_friction_issues` upserts.
- Test support may depend on the facade API, but production modules must not
  depend on test-only helpers.
