//! The execution a transition cleanup protects, read through the real hook judgment and binding
//! log: only the request's spawn nonce counts, and an unnamed source is an error, never no set.

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::services::claude_tui::source_verify::{Left, SourceHistory, Visit};
use crate::services::tui_o::writer::actor::{execution_read, execution_sources};
use crate::services::tui_o::writer::transition_port::Context;

fn context(pane: &ProducerPane, nonce: &str) -> Context {
    Context {
        tmux: pane.tmux.into(),
        nonce: nonce.into(),
        lock: Arc::default(),
    }
}

fn marker(pane: &ProducerPane) -> PathBuf {
    crate::services::tmux_common::session_temp_path(pane.tmux, "spawn_nonce").into()
}

fn sessions<'a>(names: &[&'a str]) -> BTreeSet<&'a str> {
    names.iter().copied().collect()
}

// A left launch session or a pending /clear without a pinned source fails for a candidate of it;
// another or reset spawn nonce reads no history, and an empty fresh one is no protected set.
#[tokio::test(start_paused = true)]
async fn transition_execution_is_the_request_nonces_history_and_never_an_empty_set() {
    let [a, b, x] = [(); 3].map(|_| uuid::Uuid::new_v4().to_string());
    let (_harness, a_path) = launched(&a);
    let path = |session: &str| a_path.with_file_name(format!("{session}.jsonl"));
    let pane = ProducerPane::launch(&a, &a_path);
    let spawned = std::fs::read_to_string(marker(&pane)).unwrap();
    let other = execution_read(CHANNEL, &context(&pane, "another nonce"));
    assert!(other.is_err(), "[B2:other_nonce] {other:?}");
    std::fs::write(path(&b), session_row(&b)).unwrap();
    pane.http("SessionStart", Some("clear"), &a, &b, &path(&b), 10);
    pane.http("UserPromptSubmit", None, &a, &b, &path(&b), 11);
    let b_source = source_id_for(&b, &path(&b)).unwrap();
    let (pinned, history) = execution_read(CHANNEL, &context(&pane, spawned.trim())).unwrap();
    assert_eq!(pinned, Some(b_source.clone()), "[B2:pinned] {history:?}");
    let launch = execution_sources(&history, &sessions(&[&a]));
    assert!(launch.is_err(), "[B2:launch_left_unpinned] {history:?}");
    let held = execution_sources(&history, &sessions(&["unrelated"])).unwrap();
    assert_eq!(held, [b_source.clone()], "[B2:current]");
    pane.http("SessionStart", Some("clear"), &a, &x, &path(&x), 40);
    let (_, history) = execution_read(CHANNEL, &context(&pane, spawned.trim())).unwrap();
    let awaiting = history.awaiting.as_ref().map(|visit| visit.session.clone());
    assert_eq!(awaiting, Some(x.clone()), "[B2:awaiting]");
    let unresolved = execution_sources(&history, &sessions(&[&x]));
    assert!(
        unresolved.is_err(),
        "[B2:unresolved_awaiting] {unresolved:?}"
    );
    let unnamed = execution_sources(&history, &sessions(&[""]));
    assert!(unnamed.is_err(), "[B2:unnamed_candidate] {unnamed:?}");
    let held = execution_sources(&history, &sessions(&["unrelated"])).unwrap();
    assert_eq!(held, [b_source], "[B2:still_current]");
    let fresh = uuid::Uuid::new_v4().simple().to_string();
    std::fs::write(marker(&pane), &fresh).unwrap();
    let reset = execution_read(CHANNEL, &context(&pane, spawned.trim()));
    assert!(reset.is_err(), "[B2:nonce_reset] {reset:?}");
    let (_, history) = execution_read(CHANNEL, &context(&pane, &fresh)).unwrap();
    assert!(
        history.complete && history.current.is_none(),
        "[B2:fresh] {history:?}"
    );
    let empty = execution_sources(&history, &sessions(&[&a]));
    assert!(empty.is_err(), "[B2:empty_complete] {empty:?}");
}

// A session the pane left without a pinned source fails once a candidate may be it, and a history
// not known whole fails whatever it names.
#[test]
fn transition_execution_refuses_a_left_session_without_its_pinned_source() {
    let current = SourceId {
        session_id: "c".into(),
        path: "/t/c.jsonl".into(),
        dev: 1,
        ino: 3,
    };
    let seen = Utc::now();
    let visit = Visit {
        session: "c".into(),
        pin: Some(current.clone()),
        since: None,
        seen,
    };
    let left = Left {
        pin: None,
        left_at: seen,
    };
    let history = SourceHistory {
        current: Some(visit),
        awaiting: None,
        left: BTreeMap::from([("l".to_string(), left)]),
        complete: true,
    };
    let unpinned = execution_sources(&history, &sessions(&["l"]));
    assert!(unpinned.is_err(), "[B2:pinless_left] {unpinned:?}");
    let other = execution_sources(&history, &sessions(&["k"]));
    assert_eq!(other, Ok(vec![current]), "[B2:other_candidate]");
    let partial = SourceHistory {
        complete: false,
        ..history
    };
    let partial = execution_sources(&partial, &sessions(&["k"]));
    assert!(partial.is_err(), "[B2:incomplete] {partial:?}");
}
