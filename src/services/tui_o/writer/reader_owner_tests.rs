//! Reader counting after a later execution in the same pane replaced the source's writer.

use super::*;

/// Hops through `ids`, gives each event the pane and execution nonce `run` names for its seq,
/// and reads every source to its end.
fn executions(
    harness: &Harness,
    bindings: &Arc<FakeBindings>,
    ids: &[SourceId],
    run: impl Fn(u64) -> (&'static str, String),
) {
    for (seq, pair) in (2..).zip(ids.windows(2)) {
        bindings.commit(hop(seq, &pair[0], &pair[1]));
    }
    for event in bindings.events.lock().unwrap().iter_mut() {
        let (pane, nonce) = run(event.seq);
        (event.tmux_session, event.execution_nonce) = (pane.into(), nonce);
    }
    for (i, source) in ids.iter().enumerate() {
        append(&source.path, &row(&format!("m{i}"), "body"));
    }
    let (mut sources, mut writer, mut deriver, _) = resume(harness, bindings.clone());
    let captured = sources.capture(&mut writer, &mut deriver, &mut VecDeque::new());
    captured.unwrap();
}

fn restarted(
    harness: &Harness,
    bindings: Arc<FakeBindings>,
) -> (Sources<FakeBindings>, HealthyWriter, UnitDeriver) {
    let (mut writer, health, counts) = health_writer(harness);
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings);
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    sources
        .resume(&mut writer, &mut deriver, &mut VecDeque::new())
        .unwrap();
    sources.follow(&mut writer).unwrap();
    sources.tend(&mut writer).unwrap();
    (sources, (writer, health, counts), deriver)
}

/// The published count and alarm after a restart, with every source left unretired.
fn restart_count(run: impl Fn(u64) -> (&'static str, String)) -> (usize, bool) {
    let (harness, ids, bindings) = fixture(MAX_READERS + 2);
    executions(&harness, &bindings, &ids, run);
    let (_, (mut writer, health, counts), _) = restarted(&harness, bindings);
    let alarmed = active(&health).contains(&format!("tui_o:too_many_readers:{CHANNEL}"));
    let store = writer.store();
    assert!(ids.iter().all(|s| !store.cursor(s).unwrap().retired));
    (*counts.lock().unwrap().last().unwrap(), alarmed)
}

const LAST_HOP: u64 = MAX_READERS as u64 + 2;

#[test]
fn restart_counts_only_sources_an_unreplaced_execution_may_still_write() {
    let each_hop_new = restart_count(|seq| ("tmux", format!("execution-{seq}")));
    assert_eq!(each_hop_new, (1, false), "each hop from a new execution");
    let one_execution = restart_count(|_| ("tmux", "one".into()));
    assert_eq!(one_execution, (MAX_READERS + 2, true), "one execution");
    let other_pane = restart_count(|seq| {
        (
            if seq < LAST_HOP { "other" } else { "tmux" },
            format!("e{seq}"),
        )
    });
    assert_eq!(other_pane, (MAX_READERS + 2, true), "earlier panes");
}

#[test]
fn prior_execution_chain_leaves_every_replaced_source_uncounted() {
    let nonce = |seq| if seq < LAST_HOP { "prior" } else { "current" };
    let chain = restart_count(|seq| ("tmux", nonce(seq).into()));
    assert_eq!(chain, (1, false));
}

#[test]
fn replaced_source_that_grows_counts_until_read_again() {
    let (harness, ids, bindings) = fixture(MAX_READERS + 2);
    executions(&harness, &bindings, &ids, |seq| {
        ("tmux", format!("execution-{seq}"))
    });
    let (mut sources, (mut writer, health, counts), mut deriver) = restarted(&harness, bindings);
    for source in &ids[..MAX_READERS] {
        append(&source.path, &row("late", "late write"));
    }
    sources.tend(&mut writer).unwrap();
    let reason = format!("tui_o:too_many_readers:{CHANNEL}");
    assert_eq!(counts.lock().unwrap().last(), Some(&(MAX_READERS + 1)));
    assert!(active(&health).contains(&reason));
    sources
        .capture(&mut writer, &mut deriver, &mut VecDeque::new())
        .unwrap();
    sources.tend(&mut writer).unwrap();
    assert_eq!(counts.lock().unwrap().last(), Some(&1));
    assert!(!active(&health).contains(&reason));
}
