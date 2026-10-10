use super::*;

fn owned_runtime() -> (Arc<Runtime>, Lifetime) {
    let runtime = Arc::new(Runtime::default());
    let lifetime = runtime.own();
    (runtime, lifetime)
}

fn busy(ticket: &Ticket) {
    ticket
        .with_current(|registration| {
            registration.activity = Activity::Busy;
            registration.deadline = Some(Instant::now());
        })
        .unwrap();
}

#[test]
fn b2_late_invalidate_and_retire_preserve_successor_and_other_runtime() {
    let (runtime, _lifetime) = owned_runtime();
    let a = runtime.register(1).unwrap();
    let b = runtime.register(2).unwrap();
    busy(&a);
    busy(&b);
    assert!(runtime.invalidate_if_current(&a, "transition"));
    let successor = runtime.register(1).unwrap();
    busy(&successor);
    assert!(!runtime.invalidate_if_current(&a, "late completion"));
    assert!(!runtime.retire_if_current(&a));
    let (other, _other_lifetime) = owned_runtime();
    assert!(!other.invalidate_if_current(&successor, "foreign runtime"));
    assert!(!other.retire_if_current(&successor));
    assert_eq!(
        successor.with_current(|r| (r.activity, r.deadline.is_some())),
        Some((Activity::Busy, true))
    );
    assert_eq!(b.with_current(|r| r.activity), Some(Activity::Busy));
}

#[test]
fn b2_retire_then_prune_removes_only_departed_registration() {
    let (runtime, _lifetime) = owned_runtime();
    let a = runtime.register(1).unwrap();
    let b = runtime.register(2).unwrap();
    busy(&a);
    busy(&b);
    assert!(runtime.retire_if_current(&a));
    assert!(a.incarnation().is_none());
    {
        let state = runtime.0.lock().unwrap();
        let retired = &state.channels[&1];
        assert!(retired.retired);
        assert_eq!(
            (retired.activity, retired.deadline),
            (Activity::Unknown, None)
        );
        assert!(retired.children.is_empty());
    }
    runtime.prune();
    assert!(!runtime.0.lock().unwrap().channels.contains_key(&1));
    assert_eq!(
        b.with_current(|r| (r.activity, r.deadline.is_some())),
        Some((Activity::Busy, true))
    );
    let fresh = runtime.register(1).unwrap();
    assert_eq!(
        fresh.with_current(|r| (r.activity, r.deadline)),
        Some((Activity::Unknown, None))
    );
    assert!(!runtime.retire_if_current(&a));
}

#[test]
fn b2_suspend_refuses_admission_and_resume_is_fresh_unknown() {
    let (runtime, _lifetime) = owned_runtime();
    let old = runtime.register(1).unwrap();
    let old_incarnation = old.incarnation().unwrap();
    busy(&old);
    runtime.suspend_runtime();
    assert!(runtime.register(1).is_none());
    assert!(runtime.register(2).is_none());
    assert_eq!(
        old.admit(&old_incarnation, || panic!("suspended handoff")),
        None
    );
    let suspended_token = runtime.0.lock().unwrap().channels[&1].token.clone();
    runtime.resume_fresh();
    let fresh = runtime.register(1).unwrap();
    assert!(!Arc::ptr_eq(&old.token, &fresh.token));
    assert!(!Arc::ptr_eq(&suspended_token, &fresh.token));
    assert!(!Arc::ptr_eq(
        &old_incarnation,
        &fresh.incarnation().unwrap()
    ));
    assert_eq!(
        fresh.with_current(|r| (r.activity, r.deadline)),
        Some((Activity::Unknown, None))
    );
    assert!(!runtime.invalidate_if_current(&old, "late resume result"));
    assert_eq!(old.with_current(|_| 1), None);
}

struct Child {
    polled: Arc<std::sync::atomic::AtomicUsize>,
    dropped: Option<tokio::sync::oneshot::Sender<()>>,
}

impl std::future::Future for Child {
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        self.polled
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::task::Poll::Pending
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        self.dropped.take().unwrap().send(()).ok();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn b2_lifecycle_owns_and_cancels_every_child_even_before_first_poll() {
    for transition in ["invalidate", "retire", "suspend", "drop", "panic"] {
        let (runtime, lifetime) = owned_runtime();
        let ticket = runtime.register(1).unwrap();
        let polled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut completions = Vec::new();
        for _ in 0..3 {
            let (dropped, done) = tokio::sync::oneshot::channel();
            assert!(ticket.spawn_child(Child {
                polled: polled.clone(),
                dropped: Some(dropped)
            }));
            completions.push(done);
        }
        let handles = ticket
            .with_current(|r| {
                r.children
                    .iter()
                    .map(JoinHandle::abort_handle)
                    .collect::<Vec<_>>()
            })
            .unwrap();
        match transition {
            "invalidate" => {
                assert!(runtime.invalidate_if_current(&ticket, "cancel"));
            }
            "retire" => {
                assert!(runtime.retire_if_current(&ticket));
                runtime.prune();
            }
            "suspend" => runtime.suspend_runtime(),
            "drop" => drop(lifetime),
            "panic" => {
                let result = std::panic::catch_unwind(move || {
                    let _owner = lifetime;
                    panic!("supervisor panic");
                });
                assert!(result.is_err());
            }
            _ => unreachable!(),
        }
        for done in completions {
            tokio::time::timeout(std::time::Duration::from_secs(5), done)
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(
            polled.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{transition}"
        );
        assert!(handles.iter().all(|h| h.is_finished()));
        assert!(!ticket.spawn_child(std::future::ready(())));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn b2_completed_children_are_reaped_and_running_children_cancel_on_drop() {
    let (runtime, lifetime) = owned_runtime();
    let ticket = runtime.register(1).unwrap();
    assert!(ticket.spawn_child(std::future::ready(())));
    tokio::task::yield_now().await;
    let (dropped, done) = tokio::sync::oneshot::channel();
    let polled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    assert!(ticket.spawn_child(Child {
        polled: polled.clone(),
        dropped: Some(dropped)
    }));
    assert_eq!(ticket.with_current(|r| r.children.len()), Some(1));
    tokio::task::yield_now().await;
    assert_eq!(polled.load(std::sync::atomic::Ordering::SeqCst), 1);
    drop(lifetime);
    tokio::time::timeout(std::time::Duration::from_secs(5), done)
        .await
        .unwrap()
        .unwrap();
    assert!(ticket.incarnation().is_none());
}

#[test]
fn b2_ticket_capture_and_invalidation_serialize_in_both_orders() {
    let (runtime, _lifetime) = owned_runtime();
    let ticket = runtime.register(1).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    std::thread::scope(|scope| {
        let gate = barrier.clone();
        let old = &ticket;
        let first = scope.spawn(move || {
            old.with_current(|r| {
                gate.wait();
                r.activity = Activity::Busy;
            })
        });
        barrier.wait();
        assert!(runtime.invalidate_if_current(&ticket, "after capture"));
        assert_eq!(first.join().unwrap(), Some(()));
    });
    assert_eq!(ticket.with_current(|_| panic!("late adoption")), None);
    let fresh = runtime.register(1).unwrap();
    assert_eq!(fresh.with_current(|r| r.activity), Some(Activity::Unknown));
}

#[tokio::test(flavor = "current_thread")]
async fn b2_parent_abort_before_first_poll_cancels_children_with_runtime_refs_alive() {
    let runtime = Arc::new(Runtime::default());
    assert!(runtime.register(1).is_none());
    let lifetime = runtime.own();
    let ticket = runtime.register(1).unwrap();
    let (dropped, done) = tokio::sync::oneshot::channel();
    let polled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let child = Child {
        polled: polled.clone(),
        dropped: Some(dropped),
    };
    let retained_runtime = runtime.clone();
    assert!(ticket.spawn_child(async move {
        let _retained_runtime = retained_runtime;
        child.await;
    }));
    let parent_polled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = parent_polled.clone();
    let parent = tokio::spawn(async move {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _lifetime = lifetime;
        std::future::pending::<()>().await;
    });
    parent.abort();
    assert!(parent.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(5), done)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parent_polled.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(runtime.register(1).is_none());
    assert!(ticket.incarnation().is_none());
}

#[test]
fn b2_old_owner_drop_preserves_replacement_and_closed_claim_stays_closed() {
    let runtime = Arc::new(Runtime::default());
    let old = runtime.own();
    let ticket = runtime.register(1).unwrap();
    busy(&ticket);
    let current = runtime.own();
    assert!(ticket.incarnation().is_none());
    let fresh = runtime.register(1).unwrap();
    busy(&fresh);
    drop(old);
    assert_eq!(fresh.with_current(|r| r.activity), Some(Activity::Busy));
    assert_eq!(fresh.with_current(|r| r.deadline.is_some()), Some(true));
    drop(current);
    assert!(runtime.register(1).is_none());
    let _closed_owner = runtime.own();
    assert!(runtime.register(1).is_none());
    runtime.resume_fresh();
    let resumed = runtime.register(1).unwrap();
    assert_eq!(
        resumed.with_current(|r| (r.activity, r.deadline)),
        Some((Activity::Unknown, None))
    );
}

#[test]
fn b2b1_fence_withdraws_only_existing_registrations_and_never_revives_retired_ones() {
    use super::super::entrypoints::Fence;
    let (runtime, _lifetime) = owned_runtime();
    runtime.withdraw(7, false, "absent");
    runtime.withdraw(7, true, "absent");
    assert!(!runtime.0.lock().unwrap().channels.contains_key(&7));
    let a = runtime.register(1).unwrap();
    let b = runtime.register(2).unwrap();
    busy(&a);
    busy(&b);
    runtime.withdraw(1, false, "stop");
    assert!(a.incarnation().is_none());
    let successor = runtime.register(1).unwrap();
    assert_eq!(
        successor.with_current(|r| (r.activity, r.deadline)),
        Some((Activity::Unknown, None))
    );
    runtime.withdraw(2, true, "expired");
    runtime.withdraw(2, false, "late");
    assert!(b.incarnation().is_none());
    assert!(runtime.0.lock().unwrap().channels[&2].retired);
    runtime.prune();
    assert!(!runtime.0.lock().unwrap().channels.contains_key(&2));
    assert!(successor.incarnation().is_some());
}
