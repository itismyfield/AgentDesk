//! Recap compact and suggest under the input fence, through their production bodies: a fenced
//! channel acts on nothing, an unprotected one keeps main's order, and a send keeps its permit.

use std::sync::{Arc, Mutex};

use super::RecapClearTarget;
use super::compact::*;
use super::suggest::*;
use crate::services::discord::input_runtime::fence::{self, Closing, Gate, Mode};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::QueuePersistenceContext;

const LOCAL_NODE: (&str, &str) = ("mac-mini", "mac-mini-release");

fn card(channel: u64) -> (RecapClearTarget, u64, u64) {
    let target = RecapClearTarget {
        session_key: format!("claude/token/mac-mini:g2e-{channel}"),
        provider: "claude".to_string(),
        owner_instance_id: Some("mac-mini-release".to_string()),
        turn_generation: 7,
        channel_matches: true,
        provider_matches: true,
        recap_current: true,
    };
    (target, channel + 1, channel)
}

/// A pool that never connects: these runs must not reach the database.
fn unreachable_pool() -> sqlx::PgPool {
    let options = sqlx::postgres::PgPoolOptions::new();
    options
        .connect_lazy("postgres://g2e@127.0.0.1:1/none")
        .unwrap()
}

type Log = Arc<Mutex<Vec<&'static str>>>;

type Host = Box<dyn FnOnce(&str) -> std::future::Ready<Option<String>> + Send>;
type Check = Box<dyn FnOnce() -> std::future::Ready<Result<bool, String>> + Send>;
type Live = Box<dyn FnOnce(&str) -> std::future::Ready<bool> + Send>;
type SendFn = Box<dyn FnOnce(&NativeCompactRequest) -> Result<(), String> + Send>;
type Seams = CompactSeams<Host, Check, Check, Live, SendFn>;

/// Main's boundaries, each logging its call; the send also logs its paste and Enter.
fn logged_seams(log: &Log) -> Seams {
    let note = |entry| {
        let log = log.clone();
        move || log.lock().unwrap().push(entry)
    };
    let (host, claim, fence, live) = (note("host"), note("claim"), note("fence"), note("live"));
    let (paste, enter) = (note("paste"), note("enter"));
    CompactSeams {
        host_refusal: Box::new(move |_| std::future::ready((host(), None).1)),
        legacy_claim: Box::new(move || std::future::ready((claim(), Ok(true)).1)),
        pre_inject_fence: Box::new(move || std::future::ready((fence(), Ok(true)).1)),
        is_live: Box::new(move |_| std::future::ready((live(), true).1)),
        send: Box::new(move |_| {
            paste();
            enter();
            Ok(())
        }),
    }
}

/// A protected Gate held in a mode, with its health line hidden, while this lives.
struct Held {
    _health: fence::test_health::Clear,
    _closing: Arc<Closing>,
    _frozen: Option<Box<dyn Send>>,
}

/// A protected Gate on `channel` moved to `mode`.
async fn fenced(channel: u64, mode: Mode) -> Held {
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let health = fence::test_health::Clear::new(&gate);
    let closing = Arc::new(gate.close().unwrap());
    let mut frozen = None;
    if mode != Mode::Closing {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let context = QueuePersistenceContext::new(&ProviderKind::Claude, "session", None);
        let transition = shared.freeze_legacy_queue(closing.clone(), context).await;
        frozen = Some(Box::new(transition.unwrap()) as Box<dyn Send>);
        closing.open_ledger().unwrap();
        if mode == Mode::Held {
            closing.hold().unwrap();
        }
    }
    assert_eq!(gate.mode(), mode);
    Held {
        _health: health,
        _closing: closing,
        _frozen: frozen,
    }
}

// A channel whose input is fenced refuses compact and suggest before any claim, pane probe,
// paste, Enter or enqueue, and keeps the card; only a LegacyOpen Gate admits.
#[tokio::test]
async fn g2e_fenced_recap_compact_and_suggest_act_on_nothing() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pool = unreachable_pool();
    for (n, mode) in [Mode::Closing, Mode::LedgerOpen, Mode::Held]
        .into_iter()
        .enumerate()
    {
        let channel = 6_325_960_000_000_000 + 10 * n as u64;
        let _held = fenced(channel, mode).await;
        let log = Log::default();
        let provider = ProviderKind::Claude;
        let seams = logged_seams(&log);
        let run = run_recap_compact(&provider, &pool, card(channel), LOCAL_NODE, seams).await;
        let refused = matches!(&run.outcome, RecapCompactOutcome::Refused(r) if r.contains("Mode"));
        assert!(refused, "{mode:?}: {run:?}");
        assert!(run.keeps_card(), "{mode:?}");
        assert_eq!(*log.lock().unwrap(), Vec::<&str>::new(), "{mode:?}");

        let enqueue = || {
            log.lock().unwrap().push("enqueue");
            std::future::ready(true)
        };
        let outcome = run_recap_suggest(&provider, &pool, card(channel), enqueue).await;
        assert!(
            matches!(outcome, RecapSuggestOutcome::Refused(_)),
            "{outcome:?}"
        );
        assert_eq!(*log.lock().unwrap(), Vec::<&str>::new(), "{mode:?}");
    }
}

// With no Gate on the channel the bodies are main's: the destructive claim before the probe,
// fence and send, the card dropped by the caller, and a suggest that only enqueues.
#[tokio::test]
async fn g2e_unprotected_recap_compact_and_suggest_keep_main_order() {
    let pool = unreachable_pool();
    let (channel, provider) = (6_325_960_000_000_100, ProviderKind::Claude);
    let log = Log::default();
    let seams = logged_seams(&log);
    let run = run_recap_compact(&provider, &pool, card(channel), LOCAL_NODE, seams).await;
    assert!(
        matches!(run.outcome, RecapCompactOutcome::Started(_)),
        "{run:?}"
    );
    assert!(!run.protected && !run.keeps_card());
    let order = ["host", "claim", "live", "fence", "paste", "enter"];
    assert_eq!(*log.lock().unwrap(), order);

    let enqueued = Arc::new(Mutex::new(0));
    let counted = enqueued.clone();
    let enqueue = move || {
        *counted.lock().unwrap() += 1;
        std::future::ready(true)
    };
    let outcome = run_recap_suggest(&provider, &pool, card(channel), enqueue).await;
    assert_eq!(outcome, RecapSuggestOutcome::Enqueued { consumed: false });
    assert_eq!(*enqueued.lock().unwrap(), 1);
}

// The admitted send owns its permit on the blocking worker: a close keeps waiting after the
// caller is dropped, until the send itself returns.
#[tokio::test]
async fn g2e_compact_send_keeps_its_permit_past_a_dropped_caller() {
    let channel = 6_325_960_000_000_200;
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = fence::test_health::Clear::new(&gate);
    let permit = fence::effect::admit(&ProviderKind::Claude, channel).unwrap();
    let (entered, sending) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let request = NativeCompactRequest {
        tmux_session_name: "g2e-send".to_string(),
        prompt: "/compact",
    };
    let send = move |_: &NativeCompactRequest| {
        entered.send(()).unwrap();
        released.recv().unwrap();
        Ok(())
    };
    let caller = tokio::spawn(inject_native_compact(request, permit, send));
    sending.await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let closing = gate.close().unwrap();
    let drained = closing.drain();
    tokio::pin!(drained);
    let waited = tokio::time::timeout(std::time::Duration::from_millis(200), &mut drained);
    assert!(
        waited.await.is_err(),
        "the running send still holds the drain"
    );
    release.send(()).unwrap();
    let returned = tokio::time::timeout(std::time::Duration::from_secs(10), drained);
    assert!(
        returned.await.is_ok(),
        "the returned send releases the drain"
    );
}
