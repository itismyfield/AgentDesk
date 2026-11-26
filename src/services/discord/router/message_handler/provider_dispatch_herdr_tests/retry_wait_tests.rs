//! A Pending execution whose pane is recorded without evidence is proven again before its start
//! decides anything, and a retry wait on it ends only when that evidence or the row changes.
use super::*;
use crate::services::claude_tui::busy_inject::Screen;
use crate::services::turn_orchestrator::RetryWaitCause;

fn typed(error: &str, nonce: &str, kind: &str) -> bool {
    error.starts_with(&format!("NotSent(\"pending_unattached:{nonce}:{kind}\")"))
}

/// A launch that logged its start and attached, whose stored evidence then went missing.
fn started_without_evidence(fx: &Fixture, launcher: &Arc<Launcher>) -> (String, Started) {
    let mut ports = fx.ports(launcher);
    ports.cancel_after_attach = true;
    let started = Mutex::new(None);
    let (first, _) = fx.turn(&HostedRecord::Legacy, &ports, || {
        *started.lock().unwrap() = fx.start_provider(launcher, true);
    });
    assert!(first.is_err());
    let HostedRecord::Known(mut record) = fx.record() else {
        panic!("no stored execution");
    };
    record.expected = None;
    fx.store(&record);
    (
        record.execution_nonce,
        started.into_inner().unwrap().unwrap(),
    )
}

fn placed_unproven_pending_reprobes_until_evidence(start_logged: bool) {
    let fx = Fixture::new(&format!("unproven-{start_logged}"), None);
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = match start_logged {
        true => {
            let (nonce, started) = started_without_evidence(&fx, &launcher);
            (nonce, Some(started))
        }
        false => (never_attached(&fx, &launcher), None),
    };
    launcher.unconfirmed.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        let ports = fx.ports(&launcher);
        let (refused, _) = fx.turn(&fx.record(), &ports, || {});
        let refused = refused.unwrap_err();
        assert!(typed(&refused, &nonce, "unproven"), "{refused}");
        assert!(ports.attaches.lock().unwrap().is_empty());
    }
    assert!(fx.rig.sends().is_empty());
    launcher.unconfirmed.store(false, Ordering::SeqCst);
    let ports = fx.ports(&launcher);
    let (last, _) = fx.turn(&fx.record(), &ports, || {
        if let Some(started) = &started {
            fx.answer(started);
        }
    });
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    if started.is_some() {
        assert_eq!(last, Ok(()));
        assert_eq!(fx.rig.sends(), prompt_sends());
    } else {
        let last = last.unwrap_err();
        assert!(typed(&last, &nonce, "unstarted"), "{last}");
        assert!(fx.rig.sends().is_empty());
    }
}

#[test]
fn placed_unproven_pending_reprobes_until_evidence_start_logged_false_pg() {
    placed_unproven_pending_reprobes_until_evidence(false);
}

#[test]
fn placed_unproven_pending_reprobes_until_evidence_start_logged_true_pg() {
    placed_unproven_pending_reprobes_until_evidence(true);
}

#[test]
fn started_unproven_wait_does_not_wake_until_evidence_or_due_pg() {
    let fx = Fixture::new("unproven-watch", None);
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = started_without_evidence(&fx, &launcher);
    let cause = RetryWaitCause::PendingUnproven {
        nonce: nonce.clone(),
    };
    let _runtime = fx.rt.enter();
    let (pool, owner) = (fx.pool.clone(), fx.owner.clone());
    let mut read = herdr_turn::retry_probe::read(pool, owner, CHANNEL, started.session, cause);
    for _ in 0..2 {
        assert_ne!(read(), Screen::Ready, "a logged start alone proves nothing");
    }
    let HostedRecord::Known(mut record) = fx.record() else {
        panic!("no stored execution");
    };
    record.expected = Some(expected(&nonce));
    fx.store(&record);
    assert_eq!(read(), Screen::Ready, "its evidence landed");
}

/// Rewrites the row with `moved` as its evidence lands: the row moved on between the probe's write
/// and the turn's read of it.
fn move_on_at_evidence(fx: &Fixture, moved: &str) {
    let sql = format!(
        "CREATE FUNCTION moved_on() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
           IF COALESCE(OLD.hosted_execution->'expected', 'null') = 'null'
              AND COALESCE(NEW.hosted_execution->'expected', 'null') <> 'null' THEN
             NEW.hosted_execution := {moved};
           END IF; RETURN NEW; END $$;
         CREATE TRIGGER moved_on BEFORE UPDATE ON sessions FOR EACH ROW
           EXECUTE FUNCTION moved_on();"
    );
    let sql: &'static str = Box::leak(sql.into_boxed_str());
    let created = fx.rt.block_on(sqlx::raw_sql(sql).execute(&fx.pool));
    created.unwrap();
}

#[test]
fn a_row_that_moves_on_after_a_successful_reprobe_is_refused_unproven_then_read_again_pg() {
    let other = "jsonb_set(NEW.hosted_execution || '{\"execution_nonce\":\"ffffffffffffffffffffffffffffffff\",\"expected\":null}', \
                 '{source_ref,execution_nonce}', '\"ffffffffffffffffffffffffffffffff\"')";
    let cases = [
        ("bound", "NEW.hosted_execution || '{\"state\":\"bound\"}'"),
        (
            "retired",
            "NEW.hosted_execution || '{\"state\":\"retired\"}'",
        ),
        ("nonce", other),
    ];
    for (tag, moved) in cases {
        let fx = Fixture::new(&format!("moved-{tag}"), None);
        let launcher = Arc::new(Launcher::default());
        let (nonce, started) = started_without_evidence(&fx, &launcher);
        move_on_at_evidence(&fx, moved);
        let ports = fx.ports(&launcher);
        let (refused, _) = fx.turn(&fx.record(), &ports, || {});
        let refused = refused.unwrap_err();
        assert!(typed(&refused, &nonce, "unproven"), "{tag}: {refused}");
        assert!(ports.attaches.lock().unwrap().is_empty(), "{tag}");
        assert!(fx.rig.sends().is_empty(), "{tag}");
        let cause = RetryWaitCause::PendingUnproven {
            nonce: nonce.clone(),
        };
        let _runtime = fx.rt.enter();
        let (pool, owner) = (fx.pool.clone(), fx.owner.clone());
        let mut read =
            herdr_turn::retry_probe::read(pool, owner, CHANNEL, started.session.clone(), cause);
        assert_eq!(read(), Screen::Ready, "{tag}: the moved row wakes its wait");
        drop(_runtime);
        if tag == "bound" {
            let ports = fx.ports(&launcher);
            let (delivered, _) = fx.turn(&fx.record(), &ports, || fx.answer(&started));
            assert_eq!(delivered, Ok(()), "the row read again takes the original");
            assert_eq!(fx.rig.sends(), prompt_sends());
        }
    }
}
