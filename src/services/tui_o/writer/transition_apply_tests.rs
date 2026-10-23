//! An Apply runs only on the scope its Inspect approved, compared on admission and each retirement;
//! also the probe deadline's edge and receipts past a busy candidate budget.

use axum::Json;
use serde_json::{Value, json};

use super::route_tests::{self as route, send as routed_send};
use super::*;
use crate::services::tui_o::writer::host::Readiness;
use crate::services::tui_o::writer::transition_cleanup::scope::ScopeApproval;

/// The approved scope a reply carries.
fn scope(reply: &Reply) -> &ScopeApproval {
    let scope = reply.scope.as_ref().expect("a scope");
    scope
        .as_ref()
        .unwrap_or_else(|error| panic!("no scope: {error}"))
}

/// `chain`'s request whose cutoff and checked end also cover the hop it rotates to next.
fn request_past(chain: &Chain, job: &str) -> Request {
    let mut request = chain.request(job);
    request.cutoff += 1;
    if let Some(Termination::Confirmed(end)) = &mut request.termination {
        end.last_seq += 1;
    }
    request
}

/// Binds `chain`'s pane on to a new source, so its current one becomes the next old hop.
struct Rotation {
    first: PathBuf,
    bindings: Arc<FakeBindings>,
    pane: String,
    current: SourceId,
    n: usize,
}

impl Rotation {
    fn of(chain: &Chain) -> Self {
        Self {
            first: chain.paths[0].clone(),
            bindings: Arc::clone(&chain.bindings),
            pane: chain.pane.clone(),
            current: chain.sources.last().unwrap().clone(),
            n: chain.sources.len(),
        }
    }

    fn rotate(&self) -> SourceId {
        let n = self.n;
        let body = row(&format!("n{n}"), &format!("h{n} one"));
        let (name, session) = (format!("h{n}.jsonl"), format!("h{n}"));
        let (_, next) = transcript(&self.first, &name, &session, &body);
        let cause = BindingCause::Startup;
        let bound = hop_at(
            n as u64 + 1,
            &self.pane,
            Some(&self.current),
            &next,
            cause,
            "",
        );
        self.bindings.commit(bound);
        next
    }
}

fn audited(chain: &Chain, job: &str) -> usize {
    let audited = |n: &usize| chain.audit(*n).as_deref() == Some(job);
    (0..chain.sources.len() - 1).filter(audited).count()
}

async fn finished<T>(asked: tokio::task::JoinHandle<T>) -> T {
    for _ in 0..300 {
        if asked.is_finished() {
            break;
        }
        polls(1).await;
        hooks::settle_workers();
    }
    asked.await.unwrap()
}

// An Apply under the digest of an Inspect whose scope gained a hop after the route sent it, before
// the actor took it, writes nothing; a new Inspect of the same job gives a digest that applies.
#[tokio::test(start_paused = true)]
async fn transition_apply_under_an_older_scope_than_the_actors_writes_nothing() {
    let chain = chain(2);
    let current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let request = request_past(&chain, JOB);
    let inspected = ask(&actor, Operation::Inspect(request.clone()), chain.context()).await;
    let approved = scope(&inspected).clone();
    assert_eq!(approved.hops.len(), 2, "[T1:inspected]");
    let ready = Arc::new(Readiness::default());
    let (resumed, stopping) = (watch::channel(true), watch::channel(false));
    let watched = (resumed.1, stopping.1);
    let gate = &chain.harness.gate;
    let _hosted = ready.host_port_for_test(CHANNEL, gate, actor.port.clone(), watched);
    let sent = route::SENDS.with(Cell::get);
    let operation = Operation::Apply(request.clone(), approved.digest);
    let (routed, context) = (Arc::clone(&ready), chain.context());
    let asked =
        tokio::spawn(async move { routed_send(&routed, CHANNEL, context, operation).await });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(route::SENDS.with(Cell::get) - sent, 1, "[T1:route_sent]");
    let next = Rotation::of(&chain).rotate();
    let (_, Json(answer)): (_, Json<Value>) = finished(asked).await;
    let refused = json!({"Unread": "inspect_scope_changed"});
    assert_eq!(answer["reply"]["refused"], refused, "[T1:refused] {answer}");
    assert_eq!(audited(&chain, JOB), 0, "[T1:audits]");
    assert!((0..3).all(|n| !chain.cursor(n).retired), "[T1:retired]");
    for _ in 0..40 {
        if chain
            .harness
            .port
            .posts()
            .iter()
            .any(|post| post == "h3 one")
        {
            break;
        }
        polls(1).await;
    }
    drop(current);
    let _moved = force_execution_for_tests(Ok(execution(&next, Utc::now())));
    let again = ask(&actor, Operation::Inspect(request.clone()), chain.context()).await;
    let renewed = scope(&again).clone();
    assert_eq!(renewed.hops.len(), 3, "[T1:new_scope]");
    assert_ne!(renewed.digest, approved.digest, "[T1:new_digest]");
    let applied = ask(
        &actor,
        Operation::Apply(request, renewed.digest),
        chain.context(),
    )
    .await;
    assert_eq!(applied.refused, None, "[T1:applies]");
    assert!(audited(&chain, JOB) >= 2, "[T1:applies] {:?}", applied.hops);
    halt(actor.stop, actor.task).await;
}

// All 65 hops stay in the scope, its digest unchanged once 64 are retired, so the same job's next
// Apply under the first Inspect's digest retires the last and answers with that digest.
#[tokio::test(start_paused = true)]
async fn transition_scope_keeps_every_hop_past_the_candidate_limit_across_a_partial_apply() {
    let chain = chain(65);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let request = chain.request(JOB);
    let inspect = || ask(&actor, Operation::Inspect(request.clone()), chain.context());
    let first = scope(&inspect().await).clone();
    assert_eq!(first.hops.len(), 65, "[T2:count]");
    let apply = || {
        let operation = Operation::Apply(request.clone(), first.digest);
        ask(&actor, operation, chain.context())
    };
    apply().await;
    assert_eq!(audited(&chain, JOB), 64, "[T2:partial]");
    let again = scope(&inspect().await).clone();
    let kept = (again.hops.len(), again.digest);
    assert_eq!(kept, (65, first.digest), "[T2:stable]");
    let rest = apply().await;
    assert_eq!(rest.refused, None, "[T2:same_digest]");
    assert_eq!(scope(&rest).digest, first.digest, "[T2:accepted]");
    assert_eq!(audited(&chain, JOB), 65, "[T2:rest]");
    halt(actor.stop, actor.task).await;
}

// An Inspect's scope and verdicts come from one final snapshot, a hop bound during its probes in
// both; an unreadable final snapshot, an unverified execution or its unsettled history issues none.
#[tokio::test(start_paused = true)]
async fn transition_inspect_issues_its_scope_from_its_final_snapshot_only() {
    let chain = chain(2);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let request = request_past(&chain, JOB);
    let rotation = Rotation::of(&chain);
    let asked = send(&actor, Operation::Inspect(request.clone()), chain.context());
    let reply = answered_after(asked, 1, || drop(rotation.rotate()))
        .await
        .unwrap();
    let mut judged: Vec<&String> = reply.hops.iter().map(|hop| &hop.judged.key).collect();
    let mut scoped: Vec<&String> = scope(&reply).hops.iter().map(|hop| &hop.key).collect();
    (judged.sort(), scoped.sort());
    assert_eq!(judged.len(), 3, "[T5:final_judgment]");
    assert_eq!(scoped, judged, "[T5:same_snapshot]");
    let asked = send(&actor, Operation::Inspect(request.clone()), chain.context());
    let reply = answered_after(asked, 1, || strip_hop_fields(&chain, &[0])).await;
    let reply = reply.unwrap();
    assert!(matches!(reply.execution, Some(Err(_))), "[T5:fallback]");
    assert!(
        reply.scope.is_none(),
        "[T5:fallback_scope] {:?}",
        reply.scope
    );
    halt(actor.stop, actor.task).await;
    let actor = run(&chain);
    polls(3).await;
    let _unread =
        force_execution_for_tests(Err("the pane's spawn nonce is not the request's".into()));
    let reply = ask(&actor, Operation::Inspect(request.clone()), chain.context()).await;
    assert!(matches!(reply.scope, Some(Err(_))), "[T9:unverified]");
    drop(_unread);
    for case in ["incomplete", "pinless"] {
        let (pin, mut history) = execution(chain.sources.last().unwrap(), Utc::now());
        if case == "incomplete" {
            history.complete = false;
        } else {
            let (session, seen) = (chain.sources[0].session_id.clone(), Utc::now());
            let (pin, since) = (None, None);
            history.awaiting = Some(Visit {
                session,
                pin,
                since,
                seen,
            });
        }
        let _held = force_execution_for_tests(Ok((pin, history)));
        let reply = ask(&actor, Operation::Inspect(request.clone()), chain.context()).await;
        let judged = reply.summary.is_some() && matches!(reply.execution, Some(Err(_)));
        assert!(judged, "[T9:{case}:snapshot] {:?}", reply.execution);
        assert!(
            matches!(reply.scope, Some(Err(_))),
            "[T9:{case}] {:?}",
            reply.scope
        );
    }
    halt(actor.stop, actor.task).await;
}

// A scope change after admission holds every source still to write, keeping one already retired;
// Common's check would hold them too, so this guards the final write path, not the digest alone.
#[tokio::test(start_paused = true)]
async fn transition_apply_holds_the_rest_once_its_scope_changes_after_admission() {
    for after_first in [false, true] {
        let chain = chain(3);
        let _current = chain.current();
        let actor = run(&chain);
        settle(&chain).await;
        let (rotation, rotated) = (Rotation::of(&chain), Rc::new(Cell::new(false)));
        let once = Rc::clone(&rotated);
        let _hook = hooks::on_retire(move || {
            if after_first && !once.replace(true) {
                rotation.rotate();
            }
        });
        let asked = send(
            &actor,
            chain.apply(request_past(&chain, JOB)),
            chain.context(),
        );
        let rotation = Rotation::of(&chain);
        let reply = answered_after(asked, 2, || {
            if !after_first {
                rotation.rotate();
            }
        });
        let reply = reply.await.unwrap();
        let held = Outcome::Held(Hold::InspectScopeChanged);
        let held = (0..3).filter(|n| outcomes(&reply)[&chain.key(*n)] == held);
        let written = usize::from(after_first);
        assert_eq!(audited(&chain, JOB), written, "[T6:{after_first}] written");
        assert_eq!(held.count(), 3 - written, "[T6:{after_first}] held");
        halt(actor.stop, actor.task).await;
    }
}

// Another job retiring the hops leaves this job's scope and digest as they were, and this job's
// Apply under that digest then overwrites none of its audits.
#[tokio::test(start_paused = true)]
async fn transition_scope_keeps_its_digest_when_another_job_retires_its_hops() {
    let chain = chain(2);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let request = chain.request(JOB);
    let inspect = || ask(&actor, Operation::Inspect(request.clone()), chain.context());
    let before = scope(&inspect().await).digest;
    ask(&actor, chain.apply(chain.request("other")), chain.context()).await;
    assert_eq!(audited(&chain, "other"), 2, "[T8:other_retired]");
    assert_eq!(scope(&inspect().await).digest, before, "[T8:other_job]");
    let reply = ask(&actor, Operation::Apply(request, before), chain.context()).await;
    assert_eq!(reply.refused, None, "[T8:admitted]");
    assert_eq!(audited(&chain, "other"), 2, "[T8:no_overwrite]");
    let retired = |outcome: &Outcome| matches!(outcome, Outcome::Retired(_));
    assert!(!outcomes(&reply).values().any(retired), "[T8:not_again]");
    halt(actor.stop, actor.task).await;
}

// An Apply of the ID an Inspect runs is a conflict, never its promotion: the Inspect ends as one
// and nothing is written.
#[tokio::test(start_paused = true)]
async fn transition_inspect_in_progress_is_never_promoted_by_an_apply_of_its_id() {
    let chain = chain(1);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let before = files(&chain.dir());
    let asked = send(
        &actor,
        Operation::Inspect(chain.request(JOB)),
        chain.context(),
    );
    let mut apply = Some((chain.apply(chain.request(JOB)), chain.context()));
    let mut late = None;
    let reply = answered_after(asked, 1, || {
        let (operation, context) = apply.take().unwrap();
        late = Some(send(&actor, operation, context));
    });
    let reply = reply.await.unwrap();
    let conflict = late.unwrap().await.unwrap().unwrap();
    assert_eq!(
        conflict.refused,
        Some(Refused::JobConflict),
        "[T7:inspect_in_progress]"
    );
    assert_eq!(reply.operation, Some(Kind::Inspect), "[T7:not_promoted]");
    assert_eq!(files(&chain.dir()), before, "[T7:writes]");
    halt(actor.stop, actor.task).await;
}

// Probes the worker sent before the deadline but the actor took after it stay unrecorded: the clock
// is read once they are taken, so nothing is written.
#[tokio::test(start_paused = true)]
async fn transition_probes_taken_just_past_the_deadline_write_nothing() {
    let chain = chain(1);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let kept = chain.cursor_file(0);
    let asked = send(&actor, chain.apply(chain.request(JOB)), chain.context());
    let mut skew = None;
    let reply = answered_after(asked, 2, || {
        skew = Some(hooks::skew_at_receipt(std::time::Duration::from_secs(61)));
    });
    let reply = reply.await.unwrap();
    assert_eq!(chain.audit(0), None, "[edge:audit]");
    assert_eq!(chain.cursor_file(0), kept, "[edge:cursor]");
    let outcome = outcomes(&reply)[&chain.key(0)].clone();
    assert_eq!(outcome, Outcome::Held(Hold::Limit), "[edge:limit]");
    drop(skew);
    halt(actor.stop, actor.task).await;
}

// With 64 sources still to retire busy on every Apply, each Apply still probes one receipt already
// written, a different one each time, rather than holding them all at the limit.
#[tokio::test(start_paused = true)]
async fn transition_apply_keeps_one_candidate_for_a_receipt_in_turns() {
    let chain = chain(67);
    let _current = chain.current();
    let actor = run(&chain);
    settle(&chain).await;
    let context = chain.context();
    let lock = Arc::clone(&context.lock);
    let writes = Rc::new(Cell::new(0));
    let counted = Rc::clone(&writes);
    let hook = hooks::on_retire(move || counted.set(counted.get() + 1));
    let asked = send(&actor, chain.apply(chain.request(JOB)), context);
    let mut held = None;
    for _ in 0..400 {
        if asked.is_finished() {
            break;
        }
        if held.is_none() && writes.get() >= 2 {
            held = Some(Arc::clone(&lock).try_lock_owned().unwrap());
        }
        polls(1).await;
        hooks::settle_workers();
    }
    asked.await.unwrap().unwrap();
    drop(hook);
    let receipts: Vec<String> = (0..67)
        .filter(|n| chain.audit(*n).is_some())
        .map(|n| chain.key(n))
        .collect();
    assert!(
        (2..=3).contains(&receipts.len()),
        "[receipt:fixture] {receipts:?}"
    );
    let mut probed = Vec::new();
    for _ in 0..2 {
        let context = Context {
            lock: Arc::clone(&lock),
            ..chain.context()
        };
        let reply = ask(&actor, chain.apply(chain.request(JOB)), context).await;
        let outcomes = outcomes(&reply);
        let limit = Outcome::Held(Hold::Limit);
        let taken = receipts.iter().filter(|key| outcomes[*key] != limit);
        probed.push(taken.cloned().collect::<Vec<_>>());
    }
    assert!(
        probed.iter().all(|taken| taken.len() == 1),
        "[receipt:kept] {probed:?}"
    );
    assert_ne!(probed[0], probed[1], "[receipt:turns]");
    drop(held);
    halt(actor.stop, actor.task).await;
}
