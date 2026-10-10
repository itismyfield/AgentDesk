use super::JournalEvent;
use crate::services::discord::outbound::DiscordTransportReceipt;
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(super) enum AppendResult {
    Persisted,
    DuplicateNoOp,
    InvariantConflict,
}

#[rustfmt::skip]
pub(super) enum LoadedObligationWindow { Events(Vec<JournalEvent>), Malformed }
#[derive(sqlx::FromRow)]
#[rustfmt::skip]
struct StoredJournalEvent {
    event_id: Uuid, obligation_id: Uuid, attempt_id: Option<Uuid>, event_kind: String,
    event_seq: i16, idempotency_key: Vec<u8>, canonical_payload: serde_json::Value,
    requested_channel_id: Option<String>, returned_channel_id: Option<String>,
    message_id: Option<String>,
}

pub(super) async fn load_obligation_window(
    connection: &mut sqlx::PgConnection,
    obligation_id: Uuid,
) -> Result<LoadedObligationWindow, sqlx::Error> {
    let rows = sqlx::query_as::<_, StoredJournalEvent>(
        "SELECT event_id, obligation_id, attempt_id, event_kind, event_seq,
                idempotency_key, canonical_payload, requested_channel_id,
                returned_channel_id, message_id FROM public.delivery_journal_events
          WHERE obligation_id = $1 ORDER BY event_seq, event_id",
    )
    .bind(obligation_id)
    .fetch_all(&mut *connection)
    .await?;
    Ok(match rows.into_iter().map(restore_stored_event).collect() {
        Ok(events) => LoadedObligationWindow::Events(events),
        Err(()) => LoadedObligationWindow::Malformed,
    })
}

fn restore_stored_event(row: StoredJournalEvent) -> Result<JournalEvent, ()> {
    let (kind, expected_seq, expects_attempt) = match row.event_kind.as_str() {
        "O" => ("O", 0, false),
        "A" => ("A", 1, true),
        "T" => ("T", 2, true),
        "C" => ("C", 3, true),
        "S" => ("S", 1, false),
        "U" => ("U", 2, true),
        _ => return Err(()),
    };
    if row.event_seq != expected_seq || row.attempt_id.is_some() != expects_attempt {
        return Err(());
    }
    let receipt = match (
        row.requested_channel_id,
        row.returned_channel_id,
        row.message_id,
    ) {
        (Some(requested_channel_id), Some(returned_channel_id), Some(message_id))
            if kind == "T" =>
        {
            Some(DiscordTransportReceipt {
                requested_channel_id,
                returned_channel_id,
                message_id,
            })
        }
        (None, None, None) if kind != "T" => None,
        _ => return Err(()),
    };
    Ok(JournalEvent {
        event_id: row.event_id,
        obligation_id: row.obligation_id,
        attempt_id: row.attempt_id,
        kind,
        seq: row.event_seq,
        idempotency_key: row.idempotency_key,
        canonical_payload: row.canonical_payload,
        receipt,
    })
}

/// The only production raw PostgreSQL append entry point for the delivery journal.
pub(in crate::services::discord::session_relay_sink::journal) async fn append_delivery_journal_batch(
    pool: &sqlx::PgPool,
    events: &[JournalEvent],
) -> Result<AppendResult, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    for event in events {
        use crate::services::tui_o::exact_episode::{
            EpisodeEvidence, EpisodeMetadata, SubmissionBasis,
        };
        let Ok(metadata) =
            serde_json::from_value::<EpisodeMetadata>(event.canonical_payload.clone())
        else {
            continue;
        };
        if !matches!(
            metadata.evidence,
            EpisodeEvidence::InputAttemptBegun { .. } | EpisodeEvidence::SubmissionClosed { .. }
        ) {
            continue;
        }
        // Both producers serialize before reading admission state, through the commit ACK.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("strict-submission:{}", metadata.episode))
            .execute(&mut *transaction)
            .await?;
        let payloads: Vec<serde_json::Value> = sqlx::query_scalar(
            "SELECT canonical_payload FROM public.delivery_journal_events WHERE canonical_payload->>'namespace'=$1 AND canonical_payload->>'episode'=$2",
        ).bind(crate::services::tui_o::exact_episode::STRICT_NAMESPACE)
            .bind(metadata.episode.to_string()).fetch_all(&mut *transaction).await?;
        let records = payloads
            .into_iter()
            .map(serde_json::from_value::<EpisodeMetadata>)
            .collect::<Result<Vec<_>, _>>();
        let Ok(records) = records else {
            return Ok(AppendResult::InvariantConflict);
        };
        if records.iter().any(|record| !record.supported()) {
            return Ok(AppendResult::InvariantConflict);
        }
        if records.iter().any(|record| record == &metadata) {
            continue;
        }
        let attempts: Vec<_> = records
            .iter()
            .filter_map(|record| match record.evidence {
                EpisodeEvidence::InputAttemptBegun { nonce } => Some(nonce),
                _ => None,
            })
            .collect();
        let closed = records
            .iter()
            .any(|record| matches!(record.evidence, EpisodeEvidence::SubmissionClosed { .. }));
        let pin = records.iter().find_map(|record| match &record.evidence {
            EpisodeEvidence::Pin(pin) => Some(pin),
            _ => None,
        });
        let accepted = match &metadata.evidence {
            EpisodeEvidence::InputAttemptBegun { nonce } => {
                !closed && attempts.is_empty() && !nonce.is_nil() && pin.is_some()
            }
            EpisodeEvidence::SubmissionClosed {
                generation,
                basis,
                policy_version,
            } => {
                !closed
                    && pin.is_some_and(|pin| {
                        pin.born_generation == *generation
                            && pin.context.policy_version == *policy_version
                    })
                    && !records.iter().any(|record| {
                        matches!(
                            record.evidence,
                            EpisodeEvidence::Obligation { .. }
                                | EpisodeEvidence::Attempt { .. }
                                | EpisodeEvidence::Transport { .. }
                                | EpisodeEvidence::Committed { .. }
                                | EpisodeEvidence::WholeFrontier { .. }
                                | EpisodeEvidence::Manifest(_)
                        )
                    })
                    && match basis {
                        SubmissionBasis::NoAttempt => attempts.is_empty(),
                        SubmissionBasis::GateRefused { nonce } => attempts.as_slice() == [*nonce],
                    }
            }
            _ => false,
        };
        if !accepted {
            return Ok(AppendResult::InvariantConflict);
        }
    }
    let mut inserted = false;
    for event in events {
        let receipt = event.receipt.as_ref();
        let result = sqlx::query(
            "INSERT INTO public.delivery_journal_events
             (event_id, obligation_id, attempt_id, event_kind, event_seq,
              idempotency_key, canonical_payload, requested_channel_id,
              returned_channel_id, message_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
             ON CONFLICT DO NOTHING",
        )
        .bind(event.event_id)
        .bind(event.obligation_id)
        .bind(event.attempt_id)
        .bind(event.kind)
        .bind(event.seq)
        .bind(&event.idempotency_key)
        .bind(&event.canonical_payload)
        .bind(receipt.map(|value| &value.requested_channel_id))
        .bind(receipt.map(|value| &value.returned_channel_id))
        .bind(receipt.map(|value| &value.message_id))
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() == 1 {
            inserted = true;
            continue;
        }
        let existing = sqlx::query_as::<_, (Uuid, Vec<u8>, serde_json::Value, Option<Uuid>, Option<String>, Option<String>, Option<String>)>(
            "SELECT event_id, idempotency_key, canonical_payload, attempt_id, requested_channel_id, returned_channel_id, message_id
               FROM public.delivery_journal_events
              WHERE obligation_id = $1 AND event_seq = $2",
        )
        .bind(event.obligation_id)
        .bind(event.seq)
        .fetch_one(&mut *transaction)
        .await?;
        let legacy_equal = existing.0 == event.event_id
            && existing.1 == event.idempotency_key
            && existing.2 == event.canonical_payload;
        let strict = event
            .canonical_payload
            .get("namespace")
            .and_then(serde_json::Value::as_str)
            == Some(crate::services::tui_o::exact_episode::STRICT_NAMESPACE);
        let full_equal = existing.3 == event.attempt_id
            && existing.4.as_deref() == receipt.map(|r| r.requested_channel_id.as_str())
            && existing.5.as_deref() == receipt.map(|r| r.returned_channel_id.as_str())
            && existing.6.as_deref() == receipt.map(|r| r.message_id.as_str());
        if !legacy_equal || strict && !full_equal {
            transaction.rollback().await?;
            return Ok(AppendResult::InvariantConflict);
        }
    }
    #[cfg(test)]
    for event in events {
        crate::services::tui_o::exact_submission::before_commit(
            &mut transaction,
            &event.canonical_payload,
        )
        .await?;
    }
    transaction.commit().await?;
    Ok(if inserted {
        AppendResult::Persisted
    } else {
        AppendResult::DuplicateNoOp
    })
}

#[cfg(test)]
#[rustfmt::skip]
mod tests {
    use super::*; use serde_json::json;
    fn stored(kind: &str, seq: i16, attempt_id: Option<Uuid>) -> StoredJournalEvent {
        StoredJournalEvent { event_id:Uuid::from_u128(10), obligation_id:Uuid::from_u128(11), attempt_id, event_kind:kind.into(), event_seq:seq, idempotency_key:vec![12], canonical_payload:json!({}), requested_channel_id:None, returned_channel_id:None, message_id:None }
    }
    fn receipt(mut row: StoredJournalEvent, fields: [Option<&str>; 3]) -> StoredJournalEvent {
        row.requested_channel_id=fields[0].map(str::to_string); row.returned_channel_id=fields[1].map(str::to_string); row.message_id=fields[2].map(str::to_string); row
    }
    #[test]
    fn stored_journal_event_mapping_is_closed_and_fail_closed() {
        let attempt = Uuid::from_u128(13);
        for (kind,seq,id) in [("O",0,None),("A",1,Some(attempt)),("C",3,Some(attempt)),("S",1,None),("U",2,Some(attempt))] { assert!(restore_stored_event(stored(kind,seq,id)).is_ok(),"closed {kind}"); }
        let transport=receipt(stored("T",2,Some(attempt)),[Some("10"),Some("10"),Some("20")]);
        assert_eq!(restore_stored_event(transport).unwrap().receipt.unwrap().message_id,"20");
        for row in [
            stored("future",0,None), stored("O",1,None), stored("A",1,None), stored("O",0,Some(attempt)),
            receipt(stored("T",2,Some(attempt)),[Some("10"),Some("10"),None]),
            receipt(stored("C",3,Some(attempt)),[Some("10"),Some("10"),Some("20")]),
        ] { assert!(restore_stored_event(row).is_err()); }
    }
}

#[cfg(test)]
pub(crate) mod exact_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::tui_o::exact_episode::*;

    async fn append_batch(
        pool: &sqlx::PgPool,
        events: &[JournalEvent],
    ) -> Result<AppendResult, String> {
        let observer = super::super::JournalObserver::default();
        let (ack, receiver) = tokio::sync::oneshot::channel();
        observer
            .sender()
            .send(super::super::AppendCommand {
                pool: pool.clone(),
                events: events.to_vec(),
                ack: Some(ack),
            })
            .await
            .map_err(|e| e.to_string())?;
        match receiver.await.map_err(|e| e.to_string())? {
            Err(error) if error == "strict invariant conflict" => {
                Ok(AppendResult::InvariantConflict)
            }
            result => result,
        }
    }

    pub(crate) async fn exact_duplicate_pg_full_fields_and_legacy_same_key_other_attempt() {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        for strict in [false, true] {
            let obligation = Uuid::new_v4();
            let payload = if strict {
                serde_json::json!({"namespace":STRICT_NAMESPACE})
            } else {
                serde_json::json!({})
            };
            let mut event = super::super::event(obligation, Some(Uuid::new_v4()), "A", 1, payload);
            assert!(matches!(
                append_batch(&pool, &[event.clone()]).await.unwrap(),
                AppendResult::Persisted
            ));
            event.attempt_id = Some(Uuid::new_v4());
            let result = append_batch(&pool, &[event]).await.unwrap();
            assert!(if strict {
                matches!(result, AppendResult::InvariantConflict)
            } else {
                matches!(result, AppendResult::DuplicateNoOp)
            });
            for field in 0..3 {
                let obligation = Uuid::new_v4();
                let mut event = super::super::event(
                    obligation,
                    Some(Uuid::new_v4()),
                    "T",
                    2,
                    serde_json::json!({"namespace":if strict { STRICT_NAMESPACE } else { "legacy" }}),
                );
                event.receipt = Some(DiscordTransportReceipt {
                    requested_channel_id: "10".into(),
                    returned_channel_id: "10".into(),
                    message_id: "100".into(),
                });
                assert!(matches!(
                    append_batch(&pool, &[event.clone()]).await.unwrap(),
                    AppendResult::Persisted
                ));
                let receipt = event.receipt.as_mut().unwrap();
                match field {
                    0 => receipt.requested_channel_id = "11".into(),
                    1 => receipt.returned_channel_id = "12".into(),
                    _ => receipt.message_id = "101".into(),
                }
                let result = append_batch(&pool, &[event]).await.unwrap();
                assert!(if strict {
                    matches!(result, AppendResult::InvariantConflict)
                } else {
                    matches!(result, AppendResult::DuplicateNoOp)
                });
            }
        }
    }

    pub(crate) async fn exact_namespace_pg_old_reader_and_legacy_binding_bytes_unchanged() {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let obligation = Uuid::new_v4();
        let attempt = Uuid::new_v4();
        let mut legacy = super::super::admission_events(
            obligation,
            attempt,
            serde_json::json!({"intake_outbox_id":9}),
            (10, 20),
        );
        legacy.push(super::super::transport_event(
            obligation,
            attempt,
            DiscordTransportReceipt {
                requested_channel_id: "10".into(),
                returned_channel_id: "10".into(),
                message_id: "100".into(),
            },
        ));
        legacy.push(super::super::event(
            obligation,
            Some(attempt),
            "C",
            3,
            serde_json::json!({"frontier_start":10,"frontier_end":20}),
        ));
        append_batch(&pool, &legacy).await.unwrap();
        let mut connection = pool.acquire().await.unwrap();
        let before = load_obligation_window(&mut connection, obligation)
            .await
            .unwrap();
        let bytes = |loaded: LoadedObligationWindow| match loaded {
            LoadedObligationWindow::Events(events) => serde_json::to_vec(
                &events
                    .iter()
                    .map(|e| {
                        (
                            &e.canonical_payload,
                            e.event_id,
                            e.obligation_id,
                            e.attempt_id,
                            e.kind,
                            e.seq,
                            &e.idempotency_key,
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
            LoadedObligationWindow::Malformed => panic!("legacy malformed"),
        };
        let before_bytes = bytes(before);
        let bindings = |rows: Vec<(Uuid,)>| serde_json::to_vec(&rows).unwrap();
        let query = "SELECT DISTINCT obligation_id FROM public.delivery_journal_events WHERE event_kind='O' AND canonical_payload->>'intake_outbox_id'='9' ORDER BY obligation_id";
        let before_bindings = bindings(
            sqlx::query_as(query)
                .fetch_all(&mut *connection)
                .await
                .unwrap(),
        );
        let metadata = EpisodeMetadata::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            EpisodeEvidence::SourceResolved(
                crate::services::tui_o::exact_episode::SourceIdentity {
                    incarnation: Uuid::new_v4(),
                    opener: 0,
                    digest: "strict metadata".into(),
                },
            ),
        );
        crate::services::discord::append_exact_metadata(pool.clone(), &metadata)
            .await
            .unwrap();
        assert_eq!(
            before_bytes,
            bytes(
                load_obligation_window(&mut connection, obligation)
                    .await
                    .unwrap()
            )
        );
        assert_eq!(
            before_bindings,
            bindings(
                sqlx::query_as(query)
                    .fetch_all(&mut *connection)
                    .await
                    .unwrap()
            )
        );
        let strict_obligation: Uuid = sqlx::query_scalar("SELECT obligation_id FROM public.delivery_journal_events WHERE canonical_payload->>'namespace'=$1").bind(STRICT_NAMESPACE).fetch_one(&mut *connection).await.unwrap();
        let loaded = load_obligation_window(&mut connection, strict_obligation)
            .await
            .unwrap();
        let judgment = super::super::judge_loaded_obligation_window(loaded);
        assert_eq!(judgment.delivered_outbox_id, None);
        assert!(!judgment.malformed);
    }
}

#[cfg(test)]
mod mixed_tests {
    use super::*;
    #[test]
    fn mixed_strict_rows_leave_legacy_fold_frontier_and_shadow_bytes_unchanged() {
        let id = Uuid::from_u128(900);
        let attempt = Uuid::from_u128(901);
        let mut legacy = super::super::admission_events(
            id,
            attempt,
            serde_json::json!({"intake_outbox_id":9}),
            (10, 20),
        );
        legacy.push(super::super::transport_event(
            id,
            attempt,
            DiscordTransportReceipt {
                requested_channel_id: "10".into(),
                returned_channel_id: "10".into(),
                message_id: "100".into(),
            },
        ));
        legacy.push(super::super::event(
            id,
            Some(attempt),
            "C",
            3,
            serde_json::json!({"frontier_start":10,"frontier_end":20}),
        ));
        let view = |events: &[JournalEvent]| {
            let fold = super::super::exact_delivery_predicate(events);
            let shadow = format!(
                "{:?}",
                super::super::classify_shadow_observation(events, false)
            );
            let frontier = super::super::legacy_events(events)
                .iter()
                .filter_map(super::super::event_frontier)
                .collect::<Vec<_>>();
            serde_json::to_vec(&(fold, shadow, frontier)).unwrap()
        };
        for count in [2, 3, 4] {
            let original = &legacy[..count];
            let expected = view(original);
            let mut mixed = original.to_vec();
            for metadata in crate::services::tui_o::exact_episode::tests::fixture() {
                mixed.push(super::super::event(
                    Uuid::new_v4(),
                    None,
                    "O",
                    0,
                    serde_json::to_value(metadata).unwrap(),
                ));
            }
            assert_eq!(view(&mixed), expected);
        }
        let source = [id, Uuid::from_u128(902)];
        assert_eq!(source.len(), 2, "real source obligations");
        let missing = |events: &[JournalEvent]| {
            source
                .iter()
                .filter(|obligation| {
                    let window = events
                        .iter()
                        .filter(|event| event.obligation_id == **obligation)
                        .cloned()
                        .collect::<Vec<_>>();
                    !super::super::exact_delivery_predicate(&window).0
                })
                .count()
        };
        let mut mixed = legacy.clone();
        assert_eq!(missing(&mixed), 1);
        let attempt2 = Uuid::from_u128(903);
        let complete2 = super::super::admission_events(
            source[1],
            attempt2,
            serde_json::json!({"intake_outbox_id":9}),
            (20, 30),
        );
        mixed.extend(complete2.clone());
        // Plausible strict O/A/T/C cannot satisfy an existing source obligation.
        let strict_payload = |payload: serde_json::Value| {
            let mut payload = payload;
            payload["namespace"] =
                serde_json::json!(crate::services::tui_o::exact_episode::STRICT_NAMESPACE);
            payload
        };
        for event in &complete2 {
            let mut strict = event.clone();
            strict.canonical_payload = strict_payload(strict.canonical_payload);
            mixed.push(strict);
        }
        let mut strict_t = super::super::transport_event(
            source[1],
            attempt2,
            DiscordTransportReceipt {
                requested_channel_id: "10".into(),
                returned_channel_id: "10".into(),
                message_id: "101".into(),
            },
        );
        strict_t.canonical_payload = strict_payload(strict_t.canonical_payload);
        mixed.push(strict_t);
        mixed.push(super::super::event(
            source[1],
            Some(attempt2),
            "C",
            3,
            strict_payload(serde_json::json!({"frontier_start":20,"frontier_end":30})),
        ));
        mixed.push(super::super::event(
            source[1],
            None,
            "O",
            0,
            strict_payload(serde_json::json!({"evidence":{"type":"Settled","effects":["policy"]}})),
        ));
        assert_eq!(mixed.len(), 11, "source events plus strict O/A/T/C/R");
        assert_eq!(missing(&mixed), 1, "strict metadata is not legacy delivery");
        mixed.push(super::super::transport_event(
            source[1],
            attempt2,
            DiscordTransportReceipt {
                requested_channel_id: "10".into(),
                returned_channel_id: "10".into(),
                message_id: "101".into(),
            },
        ));
        mixed.push(super::super::event(
            source[1],
            Some(attempt2),
            "C",
            3,
            serde_json::json!({"frontier_start":20,"frontier_end":30}),
        ));
        assert_eq!(
            missing(&mixed),
            0,
            "only actual legacy delivery closes missing source"
        );
    }
}
