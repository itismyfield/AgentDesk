use super::*;

pub(super) fn cancel_before_first_write() {
    let switched = crate::services::turn_host::force_codex_switch_for_test(Some(true));
    let fx = Fixture::admitted("cancel");
    let launcher = Arc::new(Launcher::default());
    let (first, _) = fx.turn(&HostedRecord::Legacy, &fx.ports(&launcher), || {
        if fx.start_provider(&launcher, false).is_some() {
            fx.cancel_now();
        }
    });
    assert!(first.unwrap_err().contains("cancel"));
    assert!(fx.rig.sends().is_empty());
    assert_eq!(fx.row(), Some(HostedState::Pending));
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    assert!(!hold_of(&nonce).exists());
    crate::services::tui_prompt_dedupe::release_herdr_execution(fx.logical(), &nonce);
    let boot = followup::restart_and_reconnect(&fx, &ProviderKind::Codex);
    assert_eq!(boot.pending, 1);
    assert_eq!(fx.row(), Some(HostedState::Pending));
    assert_eq!(
        crate::services::tui_prompt_dedupe::herdr_listed_execution(fx.logical()),
        None
    );
    assert_eq!(pane_events(&fx), 0);
    // Other launch options than the pane's refuse its first prompt before any read or write.
    let kept = options_of(&fx);
    let launched_with = std::fs::read_to_string(&kept).unwrap();
    std::fs::write(&kept, "other options").unwrap();
    let (changed, _) = fx.turn(&fx.record(), &fx.ports(&launcher), || {});
    assert!(changed.unwrap_err().contains("LaunchOptionsChanged"));
    assert!(fx.rig.sends().is_empty());
    std::fs::write(&kept, launched_with).unwrap();
    let ports = fx.ports(&launcher);
    let (second, _) = fx.turn(&fx.record(), &ports, || {
        fx.rig.answer("pane.read", screen(READY));
        fx.answer(&nonce);
    });
    assert_eq!(second, Ok(()));
    assert_eq!(fx.rig.sends(), prompt_sends());
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    assert_eq!(fx.row(), Some(HostedState::Bound));
    assert!(!hold_of(&nonce).exists());
    let starts = binding_events_since(CHANNEL, 0)
        .unwrap()
        .into_iter()
        .filter(|event| {
            event.tmux_session == fx.logical()
                && event.execution_nonce.as_deref() == Some(nonce.as_str())
                && event.cause
                    == crate::services::tui_prompt_dedupe::binding_events::BindingCause::Startup
        })
        .count();
    assert_eq!(starts, 1);
    drop(switched);
}

#[test]
fn pending_restore_never_overwrites_another_registered_nonce_pg() {
    refused_restore("other", |fx, _| {
        let other = "99887766554433221100aabbccddeeff";
        crate::services::tui_prompt_dedupe::install_herdr_execution(fx.logical(), other);
        Some(other.to_owned())
    });
}

#[test]
fn pending_restore_requires_the_current_spawn_nonce_pg() {
    refused_restore("marker", |fx, _| {
        let marker = crate::services::tmux_common::session_temp_path(fx.logical(), "spawn_nonce");
        std::fs::write(marker, "99887766554433221100aabbccddeeff").unwrap();
        None
    });
}

#[test]
fn pending_restore_rechecks_the_durable_row_before_the_first_key_pg() {
    for state in [HostedState::Bound, HostedState::Retired] {
        let tag = if state == HostedState::Retired {
            "retired"
        } else {
            "row"
        };
        refused_restore(tag, |fx, row| {
            let HostedRecord::Known(mut moved) = row.clone() else {
                panic!("no Pending row");
            };
            moved.state = state;
            fx.rt
                .block_on(
                    sqlx::query("UPDATE sessions SET hosted_execution = $1 WHERE channel_id = $2")
                        .bind(serde_json::to_value(moved).unwrap())
                        .bind(CHANNEL.to_string())
                        .execute(&fx.pool),
                )
                .unwrap();
            None
        });
    }
}

fn refused_restore(tag: &str, replace: impl FnOnce(&Fixture, &HostedRecord) -> Option<String>) {
    let switched = crate::services::turn_host::force_codex_switch_for_test(Some(true));
    let fx = Fixture::admitted(&format!("pending-restore-{tag}"));
    let launcher = Arc::new(Launcher::default());
    let (first, _) = fx.turn(&HostedRecord::Legacy, &fx.ports(&launcher), || {
        if fx.start_provider(&launcher, false).is_some() {
            fx.cancel_now();
        }
    });
    assert!(first.unwrap_err().contains("cancel"));
    let nonce = launcher.nonces.lock().unwrap()[0].clone();
    crate::services::tui_prompt_dedupe::release_herdr_execution(fx.logical(), &nonce);
    assert_eq!(
        followup::restart_and_reconnect(&fx, &ProviderKind::Codex).pending,
        1
    );
    let pending = fx.record();
    let listed = replace(&fx, &pending);
    fx.rig.answer("pane.read", screen(READY));
    let (result, _) = fx.turn(&pending, &fx.ports(&launcher), || {});
    let error = result.unwrap_err();
    let refused = match tag {
        "other" => "another execution is registered",
        "retired" => "the execution is not on a registered endpoint",
        _ => "Pending execution changed",
    };
    assert!(error.contains(refused), "{tag}: {error}");
    assert!(fx.rig.sends().is_empty(), "{tag}: no content or Enter");
    assert!(!hold_of(&nonce).exists(), "{tag}: no submission hold");
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    assert_eq!(pane_events(&fx), 0);
    assert_eq!(
        crate::services::tui_prompt_dedupe::herdr_listed_execution(fx.logical()),
        listed
    );
    if let Some(listed) = listed {
        crate::services::tui_prompt_dedupe::release_herdr_execution(fx.logical(), &listed);
    }
    drop(switched);
}
