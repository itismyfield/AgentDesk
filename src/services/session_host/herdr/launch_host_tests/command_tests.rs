use super::*;

#[test]
fn s2b1_created_pane_keeps_a_transport_proof_the_command_was_not_sent() {
    let (_dir, cwd) = workdir();
    let server = serve(created_then(cwd.display().to_string(), |serving| {
        serving.store(999, Ordering::SeqCst);
    }));
    let host = launch_host(&server);
    let request = verified_request(&server, &host, &cwd);
    SCRIPTED_E7.with(|slot| {
        slot.replace(Some((
            1,
            RestoreResume::Off {
                witness: request.restore_off_witness.clone(),
            },
        )))
    });
    let outcome = host.create(&request);
    assert!(
        server
            .methods()
            .iter()
            .any(|method| method == "workspace.create")
    );
    assert!(server.request("pane.send_input").is_none());
    assert!(
        !matches!(
            outcome,
            HerdrCreateOutcome::CreatedUnconfirmed { .. }
                | HerdrCreateOutcome::NotSent(_)
                | HerdrCreateOutcome::Indeterminate(_)
        ),
        "{outcome:?}",
    );
}
