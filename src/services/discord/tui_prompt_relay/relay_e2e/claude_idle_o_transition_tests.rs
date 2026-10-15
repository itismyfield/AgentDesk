//! Unknown hot replacements open only with content continuity and a durable output anchor.
#[cfg(test)]
#[path = "claude_idle_o_host_fixture_tests.rs"]
mod fixture;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::store::rotation::Boundary;
use crate::services::tui_o::writer::host::{self, HostParts, Readiness};
use fixture::*;
use std::sync::Arc;
use std::time::Duration;

fn prefix(f: &Fixture) -> String {
    record(&f.pane.b, "a1", "2026-10-01T02:00:00Z", "A_BODY", false)
}
fn novel(f: &Fixture, body: &str) -> String {
    record(&f.pane.b, "n1", "2026-10-01T03:00:00Z", body, false)
}
fn boundary(f: &Fixture) -> Boundary {
    f.channel()
        .rotation()
        .unwrap()
        .link(&f.b())
        .unwrap()
        .boundary
        .clone()
}
fn pending(f: &Fixture) {
    assert!(matches!(boundary(f), Boundary::Pending { .. }));
}
fn opened(f: &Fixture, from: u64) {
    let mut store = f.channel();
    let rotation = store.rotation().unwrap();
    let link = rotation.link(&f.b()).unwrap();
    assert_eq!(link.boundary, Boundary::Owed { from });
    assert_eq!(link.parent, Some(f.a()));
    assert!(
        store.ledger().boundary_evidence(&f.b()).is_some(),
        "resolution must survive reopening"
    );
    assert!(store.apply_resolved_boundaries().unwrap().is_empty());
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("state did not converge");
}

#[test]
fn running_host_discovers_unknown_b_during_a_post_and_posts_each_piece_once() {
    run(async |f| {
        std::fs::write(&f.pane.path, "").unwrap();
        let _selected = crate::services::tui_o::cutover::test_override::force_candidates(&[(
            f.pane.channel.get(),
            RuntimeHandoffKind::ClaudeTui,
        )]);
        f.gate.acquired();
        let ready = Arc::new(Readiness::default());
        let hosts = host::start(ShadowProvider::Claude, true, || HostParts {
            io: f.io.clone(),
            runtime_root: Some(f.root.clone()),
            gate: f.gate.clone(),
            readiness: ready.clone(),
        });
        until(|| ready.is_ready(f.pane.channel.get())).await;
        f.mock
            .hold_next_note
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let a = record(&f.pane.a, "a1", "2026-10-01T01:00:00Z", "A_BODY", false);
        std::fs::write(&f.pane.path, &a).unwrap();
        let held = tokio::time::timeout(Duration::from_secs(15), f.mock.note_held.notified()).await;
        assert!(
            held.is_ok(),
            "A POST must begin: alarms={:?}, posts={:?}, cursor={:?}",
            f.io.base.alarms.0.lock().unwrap(),
            f.posts(),
            f.channel().cursor(&f.a())
        );
        let before = f.channel().cursor(&f.a()).cloned();
        let b = format!("{}{}", prefix(&f), novel(&f, "N_BODY"));
        f.discover(&b).await;
        assert_eq!(f.channel().cursor(&f.a()).cloned(), before);
        f.mock.release_held_note.notify_one();
        until(|| f.posts() == ["A_BODY", "N_BODY"]).await;
        until(|| {
            use crate::services::tui_o::shadow::{UnitKey, UnitKind};
            use crate::services::tui_o::store::ledger::PieceOutcome;
            let key = UnitKey {
                channel_id: f.pane.channel.get(),
                provider: ShadowProvider::Claude,
                native_key: "n1:0".into(),
                kind: UnitKind::Body,
            };
            f.channel()
                .ledger()
                .latest_piece(&key, 0)
                .is_some_and(|(_, p)| matches!(p.outcome, Some(PieceOutcome::Posted(_))))
        })
        .await;
        assert!(ready.is_ready(f.pane.channel.get()));
        assert!(hosts.iter().all(|task| !task.is_finished()));
        opened(&f, prefix(&f).len() as u64);
        assert_eq!(
            f.channel().cursor(&f.a()).cloned(),
            before,
            "B cannot reset A cursor/hash"
        );
        f.assert_posted("A", &a);
        f.assert_posted("B", &b);
        assert_eq!(
            crate::services::discord::inflight::load_inflight_state_read_only(
                &crate::services::provider::ProviderKind::Claude,
                f.pane.channel.get()
            )
            .unwrap()
            .output_path,
            f.pane.row().output_path
        );
        assert!(
            !f.io
                .base
                .alarms
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|(_, a)| matches!(
                    a,
                    crate::services::tui_o::writer::WriterAlarm::TooManyReaders { .. }
                        | crate::services::tui_o::writer::WriterAlarm::Halted { .. }
                ))
        );
        for task in hosts {
            task.abort();
            let _ = task.await;
        }
        let mut e = f.engine();
        e.capture().unwrap();
        e.deliver(100).await;
        assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
        f.assert_posted("N", &novel(&f, "N_BODY"));
    });
}

#[test]
fn a_tail_written_during_held_post_remains_owed_after_b_discovery() {
    run(async |f| {
        f.initialize(0);
        f.gate.acquired();
        let mut e = f.engine();
        e.capture().unwrap();
        f.mock
            .hold_next_note
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let delivery = tokio::spawn(async move {
            e.deliver(1).await;
            e
        });
        tokio::time::timeout(Duration::from_secs(15), f.mock.note_held.notified())
            .await
            .expect("A POST must begin");
        let before = f.channel().cursor(&f.a()).cloned();
        let tail = record(&f.pane.a, "a2", "2026-10-01T01:30:00Z", "A_TAIL", false);
        append(&f.pane.path, &tail);
        let b = format!("{}{}", prefix(&f), novel(&f, "N_BODY"));
        f.discover(&b).await;
        assert_eq!(f.channel().cursor(&f.a()).cloned(), before);
        f.mock.release_held_note.notify_one();
        let mut e = delivery.await.unwrap();
        e.capture().unwrap();
        e.deliver(100).await;
        assert_eq!(f.posts(), ["A_BODY", "A_TAIL", "N_BODY"]);
        f.assert_posted("A tail", &tail);
        f.assert_posted("B", &b);
        drop(e);
        let mut e = f.engine();
        e.capture().unwrap();
        e.deliver(100).await;
        assert_eq!(f.posts(), ["A_BODY", "A_TAIL", "N_BODY"]);
    });
}

#[test]
fn prefix_eof_torn_n_and_pending_restart_wait_for_the_complete_record() {
    run(async |f| {
        f.initialize(0);
        f.gate.acquired();
        let mut e = f.engine();
        e.capture().unwrap();
        e.deliver(100).await;
        let p = prefix(&f);
        f.discover(&p).await;
        e.capture().unwrap();
        pending(&f);
        append(
            &f.pane.b_path(),
            &format!(
                "{{\"type\":\"mode\",\"uuid\":\"mode-id\",\"sessionId\":\"{}\"}}\n",
                f.pane.b
            ),
        );
        e.capture().unwrap();
        pending(&f);
        let metadata_len = std::fs::metadata(f.pane.b_path()).unwrap().len();
        let n = novel(&f, "N_BODY");
        append(&f.pane.b_path(), n.trim_end());
        e.capture().unwrap();
        pending(&f);
        drop(e);
        let mut e = f.engine();
        pending(&f);
        append(&f.pane.b_path(), "\n");
        e.capture().unwrap();
        e.deliver(100).await;
        opened(&f, metadata_len);
        for _ in 0..3 {
            e.capture().unwrap();
            e.deliver(100).await;
        }
        assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
        f.assert_posted("N", &novel(&f, "N_BODY"));
        f.assert_posted("B", &n);
    });
}

#[test]
fn already_spooled_n_opens_after_late_a_post_without_more_b_bytes() {
    run(async |f| {
        f.initialize(0);
        let mut e = f.engine();
        e.capture().unwrap();
        let p = prefix(&f);
        let n = novel(&f, "N_BODY");
        f.discover(&format!("{p}{n}")).await;
        e.capture().unwrap();
        pending(&f);
        assert!(f.posts().is_empty());
        drop(e);
        let mut e = f.engine();
        pending(&f);
        f.gate.acquired();
        e.deliver(100).await;
        assert_eq!(f.posts(), ["A_BODY"]);
        let bytes = std::fs::read(f.pane.b_path()).unwrap();
        e.capture().unwrap();
        e.deliver(100).await;
        assert_eq!(std::fs::read(f.pane.b_path()).unwrap(), bytes);
        opened(&f, p.len() as u64);
        assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
        f.assert_posted("N", &novel(&f, "N_BODY"));
        f.assert_posted("B", &n);
    });
}

#[test]
fn native_content_and_all_row_unit_identities_must_match() {
    for change in [
        "text",
        "tool_input",
        "tool_result",
        "array_order",
        "role",
        "meta",
        "mixed",
        "metadata",
    ] {
        run(async |f| {
            let mut a: serde_json::Value = serde_json::from_str(&record(
                &f.pane.a,
                "a1",
                "2026-10-01T01:00:00Z",
                "A_BODY",
                false,
            ))
            .unwrap();
            if change == "tool_input" {
                a["message"]["content"] = serde_json::json!([{"type":"tool_use","id":"call1","name":"Read","input":{"path":"a"}}]);
            }
            if matches!(change, "tool_result" | "array_order") {
                a["type"] = "user".into();
                a["message"]["content"] =
                    serde_json::json!([{"type":"tool_result","tool_use_id":"call1","content":"a"}]);
            }
            if change == "array_order" {
                a["message"]["content"].as_array_mut().unwrap().push(
                    serde_json::json!({"type":"tool_result","tool_use_id":"call2","content":"b"}),
                );
            }
            std::fs::write(&f.pane.path, format!("{a}\n")).unwrap();
            f.initialize(0);
            f.gate.acquired();
            let mut e = f.engine();
            e.capture().unwrap();
            e.deliver(100).await;
            let mut b = a.clone();
            b["sessionId"] = f.pane.b.clone().into();
            b["timestamp"] = "2026-10-01T02:00:00Z".into();
            b["promptId"] = "rewritten-prompt".into();
            match change {
                "text" => b["message"]["content"][0]["text"] = "changed".into(),
                "tool_input" => b["message"]["content"][0]["input"]["path"] = "changed".into(),
                "tool_result" => b["message"]["content"][0]["content"] = "changed".into(),
                "array_order" => b["message"]["content"].as_array_mut().unwrap().reverse(),
                "role" => b["message"]["role"] = "user".into(),
                "meta" => b["isMeta"] = true.into(),
                "mixed" => b["message"]["id"] = "different-native".into(),
                _ => {}
            }
            let p = format!("{b}\n");
            let n = novel(&f, "N_BODY");
            f.discover(&format!("{p}{n}")).await;
            e.capture().unwrap();
            e.deliver(100).await;
            if change == "metadata" {
                opened(&f, p.len() as u64);
                assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
                f.assert_posted("N", &novel(&f, "N_BODY"));
            } else {
                pending(&f);
                assert!(!f.posts().iter().any(|s| s == "N_BODY"));
            }
        });
    }
}

#[test]
fn only_posted_or_legitimate_excluded_anchor_and_all_pieces_count() {
    use crate::services::tui_o::store::ledger::LedgerEntry;
    use crate::services::tui_o::writer::pieces::Derived;
    for state in [
        "Prepared",
        "NotSent",
        "NotFound",
        "Rejected",
        "Ambiguous",
        "Unresolved",
        "Done",
        "Posted",
        "Excluded",
    ] {
        run(async |f| {
            f.initialize(0);
            let mut e = f.engine();
            e.capture().unwrap();
            let Derived::Piece(p) = e.owed.front().unwrap().clone() else {
                panic!("A piece");
            };
            if state == "Excluded" {
                e.writer
                    .store()
                    .append_ledger(LedgerEntry::Excluded {
                        unit_key: p.unit_key.clone(),
                        reason: "operator exclusion".into(),
                    })
                    .unwrap();
            } else {
                e.writer
                    .store()
                    .append_ledger(LedgerEntry::Prepared {
                        serial: 0,
                        unit_key: p.unit_key.clone(),
                        piece_index: 0,
                        payload: p.payload.clone(),
                        anchor_id: 100,
                        epoch: 1,
                    })
                    .unwrap();
                let entry = match state {
                    "NotSent" => Some(LedgerEntry::NotSent { serial: 0 }),
                    "NotFound" | "Done" => Some(LedgerEntry::NotFound { serial: 0 }),
                    "Rejected" => Some(LedgerEntry::Rejected {
                        serial: 0,
                        status: 403,
                    }),
                    "Ambiguous" => Some(LedgerEntry::Ambiguous {
                        serial: 0,
                        candidates: vec![101, 102],
                    }),
                    "Unresolved" => Some(LedgerEntry::Unresolved {
                        serial: 0,
                        reason: "unknown".into(),
                    }),
                    "Posted" => Some(LedgerEntry::Posted {
                        serial: 0,
                        msg_id: 101,
                    }),
                    _ => None,
                };
                if let Some(entry) = entry {
                    e.writer.store().append_ledger(entry).unwrap();
                }
            }
            if state == "Done" {
                assert_eq!(
                    e.writer.deliver(&Derived::Piece(p.clone())).await,
                    crate::services::tui_o::writer::deliver::Step::Done
                );
            }
            let p = prefix(&f);
            f.discover(&format!("{p}{}", novel(&f, "N_BODY"))).await;
            e.capture().unwrap();
            if matches!(state, "Posted" | "Excluded") {
                opened(&f, p.len() as u64);
                f.gate.acquired();
                e.deliver(100).await;
                assert_eq!(f.posts(), ["N_BODY"]);
                f.assert_posted("N", &novel(&f, "N_BODY"));
            } else {
                pending(&f);
                assert!(f.posts().is_empty(), "gate is closed");
            }
        });
    }
}

#[test]
fn retained_multipart_n_survives_apply_before_replay_and_partial_posted_restart() {
    run(async |f| {
        f.initialize(0);
        f.gate.acquired();
        let mut e = f.engine();
        e.capture().unwrap();
        e.deliver(100).await;
        let p = prefix(&f);
        let n = novel(&f, &"fragment ".repeat(700));
        f.discover(&format!("{p}{n}")).await;
        e.capture().unwrap();
        opened(&f, p.len() as u64);
        let pieces = e.owed.len();
        assert!(pieces > 1);
        drop(e);
        let mut e = f.engine();
        for _ in 0..100 {
            if f.posts().len() == 2 {
                break;
            }
            e.deliver(1).await;
        }
        assert_eq!(f.posts().len(), 2);
        drop(e);
        let mut e = f.engine();
        e.deliver(100).await;
        assert_eq!(f.posts().len(), pieces + 1);
        f.assert_posted("N", &n);
        drop(e);
        let mut e = f.engine();
        e.capture().unwrap();
        e.deliver(100).await;
        assert_eq!(f.posts().len(), pieces + 1);
    });
}

#[test]
fn durable_resolution_recovers_append_before_apply_with_the_same_parent_and_start() {
    run(async |f| {
        f.initialize(0);
        let mut e = f.engine();
        e.capture().unwrap();
        let p = prefix(&f);
        let n = novel(&f, "N_BODY");
        f.discover(&format!("{p}{n}")).await;
        e.capture().unwrap();
        pending(&f);
        let path = f
            .root
            .join("o_store")
            .join(f.pane.channel.get().to_string())
            .join("boundary");
        let pending_bytes = std::fs::read(&path).unwrap();
        f.gate.acquired();
        e.deliver(100).await;
        e.capture().unwrap();
        opened(&f, p.len() as u64);
        drop(e);
        std::fs::write(&path, pending_bytes).unwrap();
        let mut e = f.engine();
        opened(&f, p.len() as u64);
        e.deliver(100).await;
        assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
        f.assert_posted("N", &novel(&f, "N_BODY"));
        f.assert_posted("N", &n);
    });
}

#[test]
fn changed_copy_after_open_is_blocked_in_hot_and_startup_replay() {
    run(async |f| {
        f.initialize(0);
        f.gate.acquired();
        let mut e = f.engine();
        e.capture().unwrap();
        e.deliver(100).await;
        let p = prefix(&f);
        f.discover(&format!("{p}{}", novel(&f, "N_BODY"))).await;
        e.capture().unwrap();
        e.deliver(100).await;
        append(&f.pane.b_path(), &p);
        e.capture().unwrap();
        e.deliver(100).await;
        assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
        f.assert_posted("N", &novel(&f, "N_BODY"));
        append(
            &f.pane.b_path(),
            &record(&f.pane.b, "a1", "2026-10-01T04:00:00Z", "changed", false),
        );
        e.capture().unwrap();
        e.deliver(100).await;
        assert!(e.writer.is_stopped());
        drop(e);
        let mut e = f.engine();
        e.deliver(100).await;
        assert!(e.writer.is_stopped());
        assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
        f.assert_posted("N", &novel(&f, "N_BODY"));
    });
}

#[test]
fn old_markerless_pending_and_parent_responsibility_cascade_never_open() {
    run(async |f| {
        f.initialize(0);
        let mut e = f.engine();
        e.capture().unwrap();
        let p = prefix(&f);
        f.discover(&format!("{p}{}", novel(&f, "N_BODY"))).await;
        e.capture().unwrap();
        let mut rotation = f.channel().rotation().unwrap();
        rotation
            .links
            .values_mut()
            .for_each(|l| l.hot_candidate = None);
        f.channel().write_rotation(&rotation).unwrap();
        drop(e);
        f.gate.acquired();
        let mut e = f.engine();
        e.deliver(100).await;
        e.capture().unwrap();
        pending(&f);
        assert_eq!(f.posts(), ["A_BODY"]);
    });
}

#[test]
fn preserved_timestamp_is_discovery_failure_and_independent_unknown_is_pending() {
    for preserved in [true, false] {
        run(async |f| {
            f.initialize(0);
            f.gate.acquired();
            let mut e = f.engine();
            e.capture().unwrap();
            e.deliver(100).await;
            let p = if preserved {
                record(&f.pane.b, "a1", "2026-10-01T01:00:00Z", "A_BODY", false)
            } else {
                record(
                    &f.pane.b,
                    "independent",
                    "2026-10-01T02:00:00Z",
                    "A_BODY",
                    false,
                )
            };
            f.discover(&format!("{p}{}", novel(&f, "N_BODY"))).await;
            e.capture().unwrap();
            e.deliver(100).await;
            if preserved {
                assert!(f.channel().rotation().unwrap().link(&f.b()).is_none());
            } else {
                pending(&f);
            }
            assert_eq!(f.posts(), ["A_BODY"]);
        });
    }
}

#[test]
fn multipart_anchor_needs_every_piece_posted() {
    use crate::services::tui_o::store::ledger::LedgerEntry;
    use crate::services::tui_o::writer::pieces::Derived;
    run(async |f| {
        let text = "anchor ".repeat(800);
        let a = record(&f.pane.a, "a1", "2026-10-01T01:00:00Z", &text, false);
        std::fs::write(&f.pane.path, &a).unwrap();
        f.initialize(0);
        let mut e = f.engine();
        e.capture().unwrap();
        let pieces: Vec<_> = e
            .owed
            .iter()
            .filter_map(|d| match d {
                Derived::Piece(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        assert!(pieces.len() > 1);
        for (i, p) in pieces.iter().enumerate().take(pieces.len() - 1) {
            e.writer
                .store()
                .append_ledger(LedgerEntry::Prepared {
                    serial: i as u64,
                    unit_key: p.unit_key.clone(),
                    piece_index: p.index,
                    payload: p.payload.clone(),
                    anchor_id: 100 + i as u64,
                    epoch: 1,
                })
                .unwrap();
            e.writer
                .store()
                .append_ledger(LedgerEntry::Posted {
                    serial: i as u64,
                    msg_id: 101 + i as u64,
                })
                .unwrap();
        }
        let p = record(&f.pane.b, "a1", "2026-10-01T02:00:00Z", &text, false);
        f.discover(&format!("{p}{}", novel(&f, "N_BODY"))).await;
        e.capture().unwrap();
        pending(&f);
        let last = pieces.last().unwrap();
        let serial = (pieces.len() - 1) as u64;
        e.writer
            .store()
            .append_ledger(LedgerEntry::Prepared {
                serial,
                unit_key: last.unit_key.clone(),
                piece_index: last.index,
                payload: last.payload.clone(),
                anchor_id: 100 + serial,
                epoch: 1,
            })
            .unwrap();
        e.writer
            .store()
            .append_ledger(LedgerEntry::Posted {
                serial,
                msg_id: 101 + serial,
            })
            .unwrap();
        e.capture().unwrap();
        opened(&f, p.len() as u64);
        f.gate.acquired();
        e.deliver(100).await;
        assert_eq!(f.posts(), ["N_BODY"]);
        f.assert_posted("N", &novel(&f, "N_BODY"));
    });
}

#[test]
fn automatic_parent_comparison_failure_halts_hot_and_blocks_startup_without_posting_history() {
    run(async |f| {
        let h = record(
            &f.pane.a,
            "history",
            "2026-10-01T00:00:00Z",
            "HISTORY",
            false,
        );
        let p = record(&f.pane.a, "a1", "2026-10-01T01:00:00Z", "A_BODY", false);
        std::fs::write(&f.pane.path, format!("{h}{p}")).unwrap();
        f.initialize(h.len() as u64);
        f.gate.acquired();
        let mut e = f.engine();
        e.capture().unwrap();
        e.deliver(100).await;
        let prefix = prefix(&f);
        f.discover(&format!("{prefix}{}", novel(&f, "N_BODY")))
            .await;
        e.capture().unwrap();
        e.deliver(100).await;
        e.writer.store().set_retired(&f.a(), true).unwrap();
        let a_path = f.pane.path.clone();
        std::fs::rename(&a_path, a_path.with_extension("saved")).unwrap();
        let copied = record(
            &f.pane.b,
            "history",
            "2026-10-01T04:00:00Z",
            "HISTORY",
            false,
        );
        append(&f.pane.b_path(), &copied);
        assert!(
            e.capture().is_err(),
            "unreadable parent must halt the live reader"
        );
        drop(e);
        let mut e = f.engine();
        e.capture().unwrap();
        e.deliver(100).await;
        assert!(
            e.writer.is_stopped(),
            "common owe must block when parent comparison is unavailable"
        );
        assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
        f.assert_posted("N", &novel(&f, "N_BODY"));
    });
}

#[test]
fn parent_pending_and_copied_rows_outside_the_parent_start_cannot_open_cascade() {
    for parent_pending in [false, true] {
        run(async |f| {
            f.initialize(0);
            let mut e = f.engine();
            e.capture().unwrap();
            if !parent_pending {
                f.gate.acquired();
                e.deliver(100).await;
            }
            let p = prefix(&f);
            f.discover(&format!("{p}{}", novel(&f, "N_BODY"))).await;
            e.capture().unwrap();
            e.deliver(100).await;
            let c = uuid::Uuid::new_v4().to_string();
            let path = f.pane.path.with_file_name(format!("{c}.jsonl"));
            std::fs::write(
                &path,
                format!(
                    "{}{}",
                    record(&c, "a1", "2026-10-01T04:00:00Z", "A_BODY", false),
                    record(&c, "c1", "2026-10-01T05:00:00Z", "C_BODY", false)
                ),
            )
            .unwrap();
            let binding =
                crate::services::discord::tui_prompt_relay::launch_script::claude_tui_rehydrated_binding(&c, &path);
            assert!(
                crate::services::tui_prompt_dedupe::register_rehydrated_tmux_runtime_binding(
                    "claude",
                    &f.pane.tmux,
                    f.pane.channel.get(),
                    binding
                )
            );
            e.capture().unwrap();
            e.deliver(100).await;
            let c =
                crate::services::tui_o::shadow::binding_reader::source_id_for(&c, &path).unwrap();
            assert!(matches!(
                f.channel().rotation().unwrap().link(&c).unwrap().boundary,
                Boundary::Pending { .. }
            ));
            assert!(!f.posts().iter().any(|s| s == "C_BODY"));
        });
    }
}

#[test]
fn rewritten_user_prompt_id_and_json_object_order_preserve_native_content() {
    run(async |f| {
        let user = serde_json::json!({"type":"user","uuid":"prompt","promptId":"old","sessionId":f.pane.a,"timestamp":"2026-10-01T01:00:00Z","message":{"role":"user","content":"안녕 🦀"}});
        let mut tool: serde_json::Value = serde_json::from_str(&record(
            &f.pane.a,
            "tool",
            "2026-10-01T01:00:00Z",
            "",
            false,
        ))
        .unwrap();
        tool["message"]["content"] = serde_json::json!([{"type":"tool_use","id":"call","name":"Read","input":{"a":1,"b":2}}]);
        let a = record(&f.pane.a, "a1", "2026-10-01T01:00:00Z", "A_BODY", false);
        std::fs::write(&f.pane.path, format!("{user}\n{tool}\n{a}")).unwrap();
        f.initialize(0);
        f.gate.acquired();
        let mut e = f.engine();
        e.capture().unwrap();
        e.deliver(100).await;
        let mut user = user;
        user["promptId"] = "rewritten".into();
        user["sessionId"] = f.pane.b.clone().into();
        user["timestamp"] = "2026-10-01T02:00:00Z".into();
        tool["sessionId"] = f.pane.b.clone().into();
        tool["timestamp"] = "2026-10-01T02:00:00Z".into();
        let tool = tool
            .to_string()
            .replace("\"a\":1,\"b\":2", "\"b\":2,\"a\":1");
        let p = format!("{user}\n{tool}\n{}", prefix(&f));
        f.discover(&format!("{p}{}", novel(&f, "N_BODY"))).await;
        e.capture().unwrap();
        e.deliver(100).await;
        opened(&f, p.len() as u64);
        assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
        f.assert_posted("N", &novel(&f, "N_BODY"));
    });
}

#[test]
fn first_record_session_and_persisted_event_nonce_are_revalidated() {
    for bad_session in [false, true] {
        run(async |f| {
            f.initialize(0);
            f.gate.acquired();
            let mut e = f.engine();
            e.capture().unwrap();
            e.deliver(100).await;
            let p = prefix(&f);
            f.discover(&format!("{p}{}", novel(&f, "N_BODY"))).await;
            if bad_session {
                let bytes = std::fs::read_to_string(f.pane.b_path())
                    .unwrap()
                    .replacen(&f.pane.b, &f.pane.a, 1);
                std::fs::write(f.pane.b_path(), bytes).unwrap();
                e.capture().unwrap();
            } else {
                e.sources.follow(&mut e.writer).unwrap();
                let mut rotation = f.channel().rotation().unwrap();
                rotation.links.values_mut().for_each(|link| {
                    if let Some(c) = &mut link.hot_candidate {
                        c.nonce = "other-event".into();
                    }
                });
                f.channel().write_rotation(&rotation).unwrap();
                drop(e);
                e = f.engine();
                e.capture().unwrap();
            }
            pending(&f);
            e.deliver(100).await;
            assert_eq!(f.posts(), ["A_BODY"]);
        });
    }
}

#[test]
fn explicit_hook_clear_keeps_zero_start_and_parentless_resume_keeps_pending() {
    use crate::services::claude_tui::hook_server::observation_ingress::{
        IngressOutcome, observe_binding_hook,
    };
    use crate::services::tui_prompt_dedupe::binding_context::*;
    for cause in ["clear", "resume"] {
        run(async |f| {
            f.initialize(0);
            f.gate.acquired();
            let mut e = f.engine();
            e.capture().unwrap();
            e.deliver(100).await;
            std::fs::write(f.pane.b_path(), novel(&f, "N_BODY")).unwrap();
            let nonce = std::fs::read_to_string(crate::services::tmux_common::session_temp_path(
                &f.pane.tmux,
                "spawn_nonce",
            ))
            .unwrap();
            let context = execution_context("claude", &nonce).unwrap();
            let envelope = HookBindingEnvelope {
                context: CapturedContext::Captured(context),
                observed: ObservedHookProcess::default(),
            };
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
            let result = observe_binding_hook(
                "claude",
                "SessionStart",
                Some(&f.pane.a),
                Some(&f.pane.b),
                &serde_json::json!({"hook_event_name":"SessionStart","session_id":f.pane.b,"transcript_path":f.pane.b_path(),"source":cause}),
                &headers,
            );
            assert!(
                matches!(result, IngressOutcome::Durable(_)),
                "{cause}: {result:?}"
            );
            e.capture().unwrap();
            e.deliver(100).await;
            if cause == "clear" {
                assert_eq!(boundary(&f), Boundary::Owed { from: 0 });
                assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
                f.assert_posted("N", &novel(&f, "N_BODY"));
            } else {
                pending(&f);
                assert_eq!(f.posts(), ["A_BODY"]);
            }
        });
    }
}

#[test]
fn an_operator_resolution_written_after_capture_cannot_be_overwritten_by_auto_open() {
    use crate::services::tui_o::store::rotation::ResolveFrom;
    run(async |f| {
        f.initialize(0);
        let mut e = f.engine();
        e.capture().unwrap();
        let p = prefix(&f);
        f.discover(&format!("{p}{}", novel(&f, "N_BODY"))).await;
        e.capture().unwrap();
        pending(&f);
        f.store
            .record_boundary_resolved(
                f.pane.channel.get(),
                f.pane.b_path().to_str().unwrap(),
                &ResolveFrom::Offset(0),
                "operator",
            )
            .unwrap();
        f.gate.acquired();
        e.deliver(100).await;
        assert!(
            e.capture().is_err(),
            "auto proof must refuse a durable operator decision"
        );
        pending(&f);
        drop(e);
        let mut e = f.engine();
        assert_eq!(boundary(&f), Boundary::Owed { from: 0 });
        assert!(
            f.channel()
                .rotation()
                .unwrap()
                .link(&f.b())
                .unwrap()
                .parent
                .is_none()
        );
        e.deliver(100).await;
        assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
        f.assert_posted("N", &novel(&f, "N_BODY"));
    });
}

#[test]
fn an_unrecorded_entrypoint_can_be_discovered_and_content_proven() {
    run(async |f| {
        f.initialize(0);
        f.gate.acquired();
        let mut e = f.engine();
        e.capture().unwrap();
        e.deliver(100).await;
        let p = prefix(&f).replace("\"entrypoint\":\"cli\",", "");
        let n = novel(&f, "N_BODY").replace("\"entrypoint\":\"cli\",", "");
        f.discover(&format!("{p}{n}")).await;
        e.capture().unwrap();
        e.deliver(100).await;
        opened(&f, p.len() as u64);
        assert_eq!(f.posts(), ["A_BODY", "N_BODY"]);
        f.assert_posted("N", &novel(&f, "N_BODY"));
        f.assert_posted("B", &n);
    });
}

#[test]
fn a_real_empty_payload_exclusion_is_a_durable_anchor() {
    use crate::services::tui_o::writer::pieces::Derived;
    run(async |f| {
        let a = record(&f.pane.a, "a1", "2026-10-01T01:00:00Z", "", false);
        std::fs::write(&f.pane.path, &a).unwrap();
        f.initialize(0);
        f.gate.acquired();
        let mut e = f.engine();
        e.capture().unwrap();
        let Derived::Excluded { unit_key, .. } = e.owed.front().unwrap().clone() else {
            panic!("empty payload must be excluded by the real deriver");
        };
        e.deliver(100).await;
        assert!(f.posts().is_empty());
        assert_eq!(
            f.channel().ledger().excluded(&unit_key),
            Some("empty_payload")
        );
        let p = record(&f.pane.b, "a1", "2026-10-01T02:00:00Z", "", false);
        let n = novel(&f, "N_BODY");
        f.discover(&format!("{p}{n}")).await;
        e.capture().unwrap();
        e.deliver(100).await;
        opened(&f, p.len() as u64);
        assert_eq!(f.posts(), ["N_BODY"]);
        f.assert_posted("N", &n);
    });
}
