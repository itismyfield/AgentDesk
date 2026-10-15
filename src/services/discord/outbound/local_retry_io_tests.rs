use super::settle_over_http::*;
use super::*;
use crate::services::discord::outbound::o_writer_io::GatewayPort;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::store::ledger::LocalRetryMark;
use crate::services::tui_o::store::ledger::PieceOutcome;
use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};
use crate::services::tui_o::writer::deliver::{ChannelWriter, Step};
use crate::services::tui_o::writer::local_retry;
use crate::services::tui_o::writer::pieces::{Derived, PieceWork};
use chrono::Utc;

#[tokio::test]
async fn local_retry_http_uses_the_ledger_payload_and_spends_only_one_additional_post() {
    futures::future::join_all((0..7).map(|case| async move {
        let channel = 63_254_000 + case;
        let chatter = || ok(json!([message(channel, 6000, OTHER, "readable")]));
        let mut replies = vec![Reply::Drop];
        replies.push(if case == 1 {
            status("500 Server Error")
        } else {
            chatter()
        });
        replies.push(if case == 6 {
            status("403 Forbidden")
        } else {
            chatter()
        });
        if case != 6 {
            replies.extend(match case {
                0 | 1 => vec![ok(message(channel, 7000, BOT, PAYLOAD))],
                2 => vec![Reply::Drop, chatter(), chatter()],
                3 => vec![
                    Reply::Drop,
                    status("500 Server Error"),
                    status("403 Forbidden"),
                ],
                4 => vec![
                    Reply::Drop,
                    ok(json!([message(channel, 7000, BOT, PAYLOAD)])),
                    ok(json!([message(channel, 7000, BOT, PAYLOAD)])),
                ],
                _ => vec![status("403 Forbidden")],
            });
        }
        let mock = Mock::start(replies).await;
        let mut probe = TcpStream::connect(mock.base.strip_prefix("http://").unwrap())
            .await
            .unwrap();
        probe
            .write_all(b"GET / HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        probe.read_to_end(&mut response).await.unwrap();
        assert!(
            response.starts_with(b"HTTP/1.1 404"),
            "root probes must not consume Discord replies: {response:?}"
        );
        assert!(mock.probes.load(Ordering::SeqCst) >= 1);
        let runtime = tempfile::tempdir().unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
            .unwrap()
            .unwrap();
        let era = store
            .begin_era(&[channel], Utc::now(), |channel| {
                Ok(Initialized {
                    channel,
                    sources: vec![],
                    initial_anchor: 100,
                    build_digest: "test".into(),
                    at: Utc::now(),
                })
            })
            .unwrap();
        let http = serenity::HttpBuilder::new("test-token")
            .proxy(mock.base.clone())
            .ratelimiter_disabled(true)
            .build();
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        let port = Arc::new(GatewayPort::new(Arc::new(http), BOT));
        let open = || {
            ChannelWriter::new(
                store.open_channel(&era, channel).unwrap().unwrap(),
                gate.clone(),
                port.clone(),
                Lease,
                Alarms::default(),
            )
        };
        let mut writer = open();
        let item = Derived::Piece(PieceWork {
            unit_key: unit(channel, 99),
            index: 0,
            payload: PAYLOAD.into(),
        });
        assert_eq!(writer.deliver(&item).await, Step::Done, "row {case}");
        let root = writer.store().ledger().piece(0).unwrap();
        assert_eq!(root.local_retry, Some(LocalRetryMark::Budget));
        if case == 6 {
            assert!(matches!(root.outcome, Some(PieceOutcome::Unresolved(_))));
        } else {
            assert_eq!(
                root.outcome,
                Some(PieceOutcome::NotFound),
                "row {case}: {:?}",
                mock.requests()
            );
        }
        drop(writer);
        let mut writer = open();
        let when = local_retry::pending(writer.store().ledger())
            .map(|root| root.due)
            .unwrap_or_else(Utc::now);
        let step = writer.retry_local(when).await;
        assert_eq!(
            step,
            if case == 5 { Step::Stopped } else { Step::Done },
            "row {case}"
        );
        if case != 6 {
            let child = writer.store().ledger().piece(1).unwrap();
            assert_eq!(
                child.local_retry,
                Some(LocalRetryMark::Attempt { root_serial: 0 })
            );
            match case {
                0 | 1 => assert_eq!(child.outcome, Some(PieceOutcome::Posted(7000))),
                2 => assert_eq!(child.outcome, Some(PieceOutcome::NotFound)),
                3 => assert!(matches!(child.outcome, Some(PieceOutcome::Unresolved(_)))),
                4 => assert_eq!(child.outcome, Some(PieceOutcome::Ambiguous(vec![7000]))),
                _ => assert_eq!(child.outcome, Some(PieceOutcome::Rejected(403))),
            }
        }
        for _ in 0..3 {
            drop(writer);
            writer = open();
            writer.retry_local(when + chrono::Duration::days(1)).await;
            assert!(!writer.has_local_retry());
        }
        let messages = format!("/api/v10/channels/{channel}/messages");
        let bodies: Vec<_> = mock
            .requests()
            .into_iter()
            .filter(|(path, body)| path == &messages && !body.is_empty())
            .map(|(_, body)| serde_json::from_slice::<Value>(&body).unwrap()["content"].clone())
            .collect();
        assert_eq!(
            bodies,
            vec![Value::from(PAYLOAD); if case == 6 { 1 } else { 2 }],
            "row {case}"
        );
    }))
    .await;
}
