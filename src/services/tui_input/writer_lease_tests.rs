//! Every legacy tmux content writer against an owner-held target: a thread without the owner's
//! scope and a write to another target fail before any byte; unheld targets write as main.
use crate::services::claude_tui::host_input::InputTransport;
use crate::services::claude_tui::owner_input;
use crate::services::session_host::{HostSessionRef, InteractiveSessionHost, TmuxHost};
use crate::services::tui_input::submission_tmux::SubmissionTmux;
use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
use std::sync::Arc;

type Write = fn(&str) -> Result<(), String>;

const WRITERS: [(&str, Write); 5] = [
    ("submission literal", |s| {
        InputTransport::send_literal(&mut SubmissionTmux, s, "lit").map(drop)
    }),
    ("submission paste", |s| {
        InputTransport::load_buffer(&mut SubmissionTmux, "a4b-sub", "sub")?;
        InputTransport::paste_buffer(&mut SubmissionTmux, s, "a4b-sub", true).map(drop)
    }),
    ("legacy literal", |s| {
        TmuxHost.send_literal(s, "leg").map(drop)
    }),
    ("legacy paste", |s| {
        crate::services::platform::tmux::load_buffer("a4b-leg", "pas")?;
        TmuxHost.paste_buffer(s, "a4b-leg", true).map(drop)
    }),
    ("session host text", |s| {
        let sent = TmuxHost.send_text(HostSessionRef::tmux(s), "host");
        match sent {
            Ok(crate::services::session_host::HostMutation::Confirmed) => Ok(()),
            other => Err(format!("{other:?}")),
        }
    }),
];

fn owner_scope<R>(owner: &str, run: impl FnOnce() -> R) -> R {
    owner_input::with_scope(
        "claude".into(),
        serde_json::json!({"host":"tmux","owner":owner,"root":"/a4b","execution_nonce":"a4b"}),
        742702,
        Arc::new(|_| Ok(())),
        Arc::new(|_, _| Ok(())),
        Arc::new(|_| Ok(())),
        run,
    )
}

fn received(f: &Fixture, name: &str) -> String {
    std::thread::sleep(std::time::Duration::from_millis(200));
    std::fs::read_to_string(f.root.path().join(name).join("received")).unwrap_or_default()
}

/// Inside an owner's scope its target is held: the same writers on a spawned thread, and the
/// owner's thread writing another target, are refused unsent; once the owner ends, both run.
#[test]
fn held_tmux_target_refuses_unscoped_and_foreign_content_at_every_writer() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    f.session("a4b-held");
    f.session("a4b-free");
    owner_scope("a4b-held", || {
        for (name, write) in WRITERS {
            assert!(write("a4b-free").is_err(), "{name}: foreign target");
            let spawned = std::thread::spawn(move || write("a4b-held"));
            assert!(
                spawned.join().unwrap().is_err(),
                "{name}: thread lacks scope"
            );
        }
    });
    assert_eq!(received(&f, "a4b-held"), "");
    assert_eq!(received(&f, "a4b-free"), "");
    for (name, write) in WRITERS {
        assert_eq!(write("a4b-free"), Ok(()), "{name}: unheld target");
        let spawned = std::thread::spawn(move || write("a4b-held"));
        assert_eq!(spawned.join().unwrap(), Ok(()), "{name}: released target");
    }
    let free = received(&f, "a4b-free");
    for text in ["lit", "sub", "leg", "pas", "host"] {
        assert!(free.contains(text), "{text} in {free:?}");
    }
}

/// A Codex argv launch for an owner-held session from a thread without the owner's scope never
/// creates the session.
#[test]
fn held_tmux_target_refuses_an_unscoped_argv_launch() {
    let context: crate::services::tui_prompt_dedupe::binding_context::BindingContext =
        serde_json::from_value(serde_json::json!({"schema":1,"provider":"codex","created_at":chrono::Utc::now(),"execution_nonce":"a4b","tmux_session":"a4b-argv","channel_id":742702,"owner_runtime_root":"/a4b","host":"argv-test","expected_native_session_id":null,"launch_mode":"fresh","provider_root":null})).unwrap();
    let context = Arc::new(context);
    owner_scope("a4b-argv", || {
        let context = context.clone();
        let spawned = std::thread::spawn(move || {
            owner_input::argv_payload(&context, || -> Result<(), String> {
                panic!("an unscoped launch reached the session")
            })
        });
        assert!(spawned.join().unwrap().is_err());
    });
    assert_eq!(owner_input::argv_payload(&context, || Ok(7)), Ok(7));
}
