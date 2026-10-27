use super::*;

#[test]
#[cfg(unix)]
fn bundle_preparation_scope_carries_input_run_without_duplicate_owner_callbacks() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Observed(AtomicUsize);
    impl guarded_outcome::Owner for Observed {
        fn started(&self) {}
        fn finished(&self, _: &host_input::InputRun) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    for run in [
        host_input::InputRun::Applied,
        host_input::InputRun::Cancelled { confirmed: 1 },
    ] {
        let owner = Arc::new(Observed(AtomicUsize::new(0)));
        let ((result, observed), _) = guarded_outcome::with_owner(Some(owner.clone()), || {
            (
                guarded_outcome::capture(|| {
                    guarded_outcome::capture_preparation(|| {
                        guarded_outcome::finish(&run);
                        if run == host_input::InputRun::Applied {
                            Ok(())
                        } else {
                            Err(SubmissionError::Held(PreparationHold::uncertain(
                                "scope-control",
                                "uncertain".into(),
                            )))
                        }
                    })
                }),
                (),
            )
        });
        assert_eq!(
            observed,
            Some(run.clone()),
            "the same owner's disposition reaches the outer capture"
        );
        assert_eq!(
            owner.0.load(Ordering::SeqCst),
            1,
            "a scope transfer never notifies twice"
        );
        if run != host_input::InputRun::Applied {
            assert!(matches!(result, Err(SubmissionError::Held(ref hold)) if !hold.may_requeue()));
        }
    }
}
