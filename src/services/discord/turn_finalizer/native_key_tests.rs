use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::claude_tui::owner_input;
use crate::services::discord::{inflight, turn_finalizer::native_prepare::Handle};
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, PANE};
use crate::services::tui_input::human_frame::history::v2::native::Outcome;
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::pin::Pin;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord::turn_finalizer) enum KeyRead {
    Ticket,
    Settlement,
}

type Bind = Pin<Box<dyn Future<Output = ()> + Send>>;
thread_local! {
    static BIND_ON_READ: RefCell<Option<(KeyRead, Bind)>> = const { RefCell::new(None) };
}

pub(in crate::services::discord::turn_finalizer) async fn bind_at(read: KeyRead) {
    let bind = BIND_ON_READ.with(|slot| {
        let mut hook = slot.borrow_mut();
        if hook.as_ref().is_some_and(|(at, _)| *at == read) {
            hook.take().map(|(_, action)| action)
        } else {
            None
        }
    });
    if let Some(bind) = bind {
        bind.await;
    }
}

pub(in crate::services::discord::turn_finalizer) async fn bind_before_key_witness() {
    bind_at(KeyRead::Ticket).await;
    bind_at(KeyRead::Settlement).await;
}

enum Order {
    KeyFirst,
    BindFirst,
    During(KeyRead),
}

async fn key_and_bind(order: Order) {
    let root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = 1;
    let f = Fixture::with_runtime(root, shared.clone()).await;
    let episode = f.ticket.owning_episode.as_ref().unwrap();
    let channel = ChannelId::new(f.ticket.channel_id);
    let mut row = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        channel.get(),
        None,
        7,
        episode.user_message_id,
        episode.user_message_id,
        "cold request".into(),
        f.ticket.key["attempt"]["source"]["expected_session_id"]
            .as_str()
            .map(str::to_owned),
        Some(f.logical.clone()),
        Some(f.path.display().to_string()),
        None,
        0,
    );
    row.born_generation = episode.generation;
    row.turn_nonce = Some(episode.turn_nonce.clone());
    row.hosted_execution_nonce = Some(f.nonce.clone());
    inflight::save_inflight_state(&row).unwrap();

    let actor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        episode.turn_nonce.clone(),
    )));
    shared
        .mailbox(channel)
        .restore_active_turn(
            actor.clone(),
            poise::serenity_prelude::UserId::new(7),
            episode.user_message_id.into(),
        )
        .await;
    shared
        .restart
        .global_active
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let key = TurnKey::new(channel, episode.user_message_id, episode.generation)
        .with_episode_nonce(Some(&episode.turn_nonce));
    shared.turn_finalizer.register_start(
        key,
        ProviderKind::Claude,
        inflight::RelayOwnerKind::None,
        &shared,
    );
    let handle = Handle::prepare(
        &shared,
        key,
        Some(actor),
        f.history.clone(),
        f.namespace.clone(),
        f.ticket.clone(),
    )
    .await
    .unwrap();
    pin_source(&shared, &f.namespace, &f.ticket).await.unwrap();
    let port = f.port();
    let (live, _) = port.custody().await.unwrap();
    assert_eq!(live.len(), 1, "O takes custody before the witness starts");
    let (tx, sent) = tokio::sync::oneshot::channel();
    let (proceed, finish) = std::sync::mpsc::channel();
    let rig = HerdrRig::start();
    let socket = rig.socket().to_path_buf();
    let target = f.ticket.key["attempt"]["target"].clone();
    let producer = tokio::task::spawn_blocking(move || {
        owner_input::with_scope(
            "claude".into(),
            target,
            channel.get(),
            Arc::new(|_| Ok(())),
            Arc::new(|_, _| Ok(())),
            Arc::new(|_| Ok(())),
            || {
                let mut pane = UnixStream::connect(socket).unwrap();
                pane.set_read_timeout(Some(crate::test_support::wait::WAIT_CAP))
                    .unwrap();
                owner_input::keyed();
                writeln!(
                    pane,
                    "{}",
                    json!({"id":"first-key","method":"pane.send_keys",
                    "params":{"pane_id":PANE,"keys":["enter"]}})
                )
                .unwrap();
                let mut reply = String::new();
                BufReader::new(pane).read_line(&mut reply).unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&reply).unwrap()["id"],
                    "first-key"
                );
                tx.send(()).unwrap();
                finish
                    .recv_timeout(crate::test_support::wait::WAIT_CAP)
                    .unwrap();
                handle.finish()
            },
        )
    });
    crate::test_support::wait::await_within(sent, crate::test_support::wait::WAIT_CAP)
        .await
        .unwrap();
    assert_eq!(rig.sends().len(), 1);
    f.transcript();
    f.project_submitted().await;

    let bind = {
        let history = f.history.clone();
        let ticket = f.ticket.clone();
        let namespace = f.namespace.clone();
        async move {
            port.bind_source(&namespace, &ticket, &history)
                .await
                .unwrap()
        }
    };
    let committed = || {
        let fresh = Registry::at(f.history_path.clone());
        let current = fresh.native_ticket(&f.namespace, &f.ticket.key).unwrap();
        fresh.native_settlement(&f.namespace, &current).unwrap()
    };
    let later = match order {
        Order::KeyFirst => Some(bind),
        Order::BindFirst => {
            bind.await;
            assert!(committed().ticket.source.is_some());
            None
        }
        Order::During(at) => {
            BIND_ON_READ.with(|slot| {
                assert!(slot.replace(Some((at, Box::pin(bind)))).is_none());
            });
            None
        }
    };
    assert_eq!(
        committed().extra.get("any_key"),
        None,
        "a key needs the finalizer witness"
    );
    proceed.send(()).unwrap();
    let result =
        crate::test_support::wait::await_within(producer, crate::test_support::wait::WAIT_CAP)
            .await
            .unwrap();
    assert!(
        BIND_ON_READ.with(|slot| slot.take().is_none()),
        "the requested boundary ran"
    );
    result.expect("one sent key gets a durable AnyKey acknowledgement");
    assert_eq!(committed().extra["any_key"], true);
    if let Some(bind) = later {
        assert!(committed().ticket.source.is_none());
        bind.await;
    }
    f.assert_bound().await;
    let durable = committed();
    assert_eq!(durable.extra["any_key"], true);
    assert!(durable.ticket.source.is_some() && durable.outcome.is_none());
    finish_with_o(&f, &shared).await;
    assert_eq!(committed().ticket.source, durable.ticket.source);
    assert_eq!(
        rig.sends().len(),
        1,
        "source binding and cleanup never send a second key"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn native_8148_coldsource_any_key_and_bind_preserve_custody_in_both_orders_pg() {
    key_and_bind(Order::KeyFirst).await;
    key_and_bind(Order::BindFirst).await;
}

#[tokio::test]
async fn native_8148_coldsource_bind_after_ticket_read_keeps_any_key_pg() {
    key_and_bind(Order::During(KeyRead::Ticket)).await;
}

#[tokio::test]
async fn native_8148_coldsource_bind_after_settlement_read_keeps_any_key_pg() {
    key_and_bind(Order::During(KeyRead::Settlement)).await;
}

#[tokio::test]
async fn native_8148_coldsource_key_witnesses_pin_identity_and_preserve_exclusivity() {
    for (first, outcome) in [(true, false), (false, false), (false, true)] {
        let f = Fixture::new().await;
        pin_source(&f.shared, &f.namespace, &f.ticket)
            .await
            .unwrap();
        f.transcript();
        f.port()
            .bind_source(&f.namespace, &f.ticket, &f.history)
            .await
            .unwrap();
        let fresh = Registry::at(f.history_path.clone());
        let bound = fresh.native_ticket(&f.namespace, &f.ticket.key).unwrap();
        let saved = || fresh.native_settlement(&f.namespace, &bound).unwrap();
        let any_key = |namespace, ticket| fresh.note_native_any_key(namespace, ticket);
        let not_sent = |namespace, ticket| fresh.try_native_not_sent(namespace, ticket);
        let mut wrong_key = bound.clone();
        wrong_key.key["attempt"]["nonce"] = json!("foreign");
        let mut wrong_channel = bound.clone();
        wrong_channel.channel_id += 1;
        let mut wrong_episode = bound.clone();
        wrong_episode.owning_episode.as_mut().unwrap().generation += 1;
        let mut wrong_extra = bound.clone();
        wrong_extra.extra.insert("foreign".into(), json!(true));
        let mut wrong_source = bound.clone();
        wrong_source.source.as_mut().unwrap().offset += 1;
        for (namespace, ticket) in [
            ("claude/foreign", &bound),
            (&f.namespace, &wrong_key),
            (&f.namespace, &wrong_channel),
            (&f.namespace, &wrong_episode),
            (&f.namespace, &wrong_extra),
            (&f.namespace, &wrong_source),
        ] {
            assert!(any_key(namespace, ticket).is_err());
            assert!(not_sent(namespace, ticket).is_err());
        }
        assert!(saved().extra.is_empty());
        if outcome {
            let terminal = Outcome {
                disposition: Disposition::Cancelled,
                range: None,
                extra: Default::default(),
            };
            fresh
                .record_native_outcome(&f.namespace, &bound, &terminal)
                .unwrap();
            assert!(any_key(&f.namespace, &f.ticket).is_err());
            assert!(not_sent(&f.namespace, &f.ticket).is_err());
            assert_eq!(saved().outcome.as_ref(), Some(&terminal));
        } else if first {
            any_key(&f.namespace, &f.ticket).unwrap();
            assert!(not_sent(&f.namespace, &f.ticket).is_err());
            assert_eq!(
                saved().extra,
                json!({"any_key": true}).as_object().unwrap().clone()
            );
        } else {
            not_sent(&f.namespace, &f.ticket).unwrap();
            assert!(any_key(&f.namespace, &f.ticket).is_err());
            assert_eq!(
                saved().extra,
                json!({"not_sent": true}).as_object().unwrap().clone()
            );
        }
        assert_eq!(saved().ticket, bound);
    }
}
