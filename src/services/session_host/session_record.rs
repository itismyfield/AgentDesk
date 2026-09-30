//! Sessions-row witness for the target resolver. Only a found row with no hosted
//! record proves a legacy session; a missing row, a failed read or a trace never does.
#![cfg_attr(not(test), allow(dead_code))]

use super::model::HostKind;
use super::resolve::HostWitness;
use crate::db::dispatched_sessions::hosted_execution::{HostedLookup, HostedRecord};

pub(crate) fn session_record_witness(lookup: &HostedLookup) -> HostWitness {
    match lookup {
        HostedLookup::Found(found) => match &found.record {
            HostedRecord::Legacy => HostWitness::LegacyRow,
            // Only a Herdr launch writes a record, so even a retired one keeps the row off tmux.
            HostedRecord::Known(record) => HostWitness::Known {
                kind: HostKind::Herdr,
                target: record.location.as_ref().map(|at| at.pane_id.clone()),
            },
            HostedRecord::Unknown(raw) => HostWitness::Unrecognized(raw.to_string()),
        },
        HostedLookup::Missing => HostWitness::NoRow,
        HostedLookup::Unknown(detail) => HostWitness::ReadFailed(detail.clone()),
        HostedLookup::Conflict(kind) => HostWitness::RowConflict(format!("{kind:?}")),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use sqlx::PgPool;

    use super::*;
    use crate::db::dispatched_session_canonical_identity::{
        CanonicalSessionIdentity, SessionIdentityKind, upsert_hook_session_with_identity_pg,
    };
    use crate::db::dispatched_sessions::HookSessionUpsert;
    use crate::db::dispatched_sessions::hosted_execution::tests::{
        TOKEN, future_schema, owner, pending, record, wire,
    };
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedLookupKey, HostedState, load_hosted_execution_pg,
    };
    use crate::services::session_host::{
        AutomaticEffect, GuardRefusal, GuardVerdict, SessionTargetEvidence,
        SessionTargetEvidenceSource, SessionTargetInput, StateChange, guard_first_state_change,
        resolve_session_target,
    };

    const NAME: &str = "AgentDesk-claude-adk-cc";

    struct RecordOnly(HostWitness);

    impl SessionTargetEvidenceSource for RecordOnly {
        fn read_evidence(&self, _input: &SessionTargetInput) -> SessionTargetEvidence {
            SessionTargetEvidence {
                session_name: Some(NAME.to_string()),
                session_record: self.0.clone(),
                inflight_locator: HostWitness::Absent,
                host_marker: HostWitness::Absent,
                ..SessionTargetEvidence::unread()
            }
        }
    }

    async fn seed(pool: &PgPool, key: &str, channel_id: &str, raw: Option<Value>) {
        let params = HookSessionUpsert {
            session_key: key,
            instance_id: Some("test-node"),
            agent_id: None,
            provider: "claude",
            status: "idle",
            session_info: None,
            model: None,
            tokens: None,
            cwd: None,
            active_dispatch_id: None,
            thread_channel_id: None,
            channel_id: Some(channel_id),
            claude_session_id: None,
            raw_provider_session_id: None,
            turn_start_nonce: None,
            dispatched_origin: false,
        };
        let identity = CanonicalSessionIdentity {
            kind: SessionIdentityKind::DiscordChannel,
            discord_token_hash: TOKEN,
            channel_id,
        };
        upsert_hook_session_with_identity_pg(pool, params, Some(identity))
            .await
            .unwrap();
        sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE session_key = $1")
            .bind(key)
            .bind(raw)
            .execute(pool)
            .await
            .unwrap();
    }

    // Every lookup outcome read from a real sessions table, through resolve and guard.
    #[tokio::test]
    async fn only_a_found_null_record_admits_the_legacy_clear_pg() {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let key = |name: &str| format!("claude/{TOKEN}/mac-mini:{name}");
        let foreign = wire(&pending(&owner("1479671301387059999"), "n9"));
        let rows = [
            ("legacy", "1479671301387059301", None),
            (
                "bound",
                "1479671301387059302",
                Some(wire(&record(
                    &owner("1479671301387059302"),
                    "n1",
                    HostedState::Bound,
                ))),
            ),
            (
                "retired",
                "1479671301387059303",
                Some(wire(&record(
                    &owner("1479671301387059303"),
                    "n2",
                    HostedState::Retired,
                ))),
            ),
            (
                "pending",
                "1479671301387059304",
                Some(wire(&pending(&owner("1479671301387059304"), "n3"))),
            ),
            (
                "future",
                "1479671301387059305",
                Some(future_schema(&owner("1479671301387059305"))),
            ),
            ("foreign", "1479671301387059306", Some(foreign)),
        ];
        for (name, channel, raw) in rows {
            seed(&pool, &key(name), channel, raw).await;
        }
        let herdr = |pane: Option<&str>| HostWitness::Known {
            kind: HostKind::Herdr,
            target: pane.map(str::to_string),
        };
        let (proceed, defer) = (
            GuardVerdict::Proceed,
            GuardVerdict::DeferredToExistingRecovery,
        );
        let unknown = GuardVerdict::Refused(GuardRefusal::UnknownHost);
        let cases = [
            (key("legacy"), HostWitness::LegacyRow, proceed),
            (key("bound"), herdr(Some("pane-1")), defer),
            (key("retired"), herdr(Some("pane-1")), defer),
            (key("pending"), herdr(None), unknown),
            (key("absent"), HostWitness::NoRow, unknown),
            (
                key("foreign"),
                HostWitness::RowConflict("OwnershipMismatch".into()),
                unknown,
            ),
        ];
        for (session_key, expected, admitted) in cases {
            let lookup =
                load_hosted_execution_pg(&pool, HostedLookupKey::SessionKey(&session_key)).await;
            let witness = session_record_witness(&lookup);
            assert_eq!(witness, expected, "{session_key}: {lookup:?}");
            let resolved = resolve_session_target(
                SessionTargetInput::RawName(NAME.to_string()),
                &RecordOnly(witness),
            );
            let clear = StateChange::Automatic {
                effect: AutomaticEffect::Clear,
                observed: None,
            };
            assert_eq!(
                guard_first_state_change(&resolved, clear),
                admitted,
                "{session_key}"
            );
        }
        for (session_key, unreadable) in [(key("future"), true), (" ".to_string(), false)] {
            let lookup =
                load_hosted_execution_pg(&pool, HostedLookupKey::SessionKey(&session_key)).await;
            let witness = session_record_witness(&lookup);
            let matched = match witness {
                HostWitness::Unrecognized(_) => unreadable,
                HostWitness::ReadFailed(_) => !unreadable,
                _ => false,
            };
            assert!(matched, "{session_key:?}: {witness:?}");
        }
        pool.close().await;
        db.drop().await;
    }
}
