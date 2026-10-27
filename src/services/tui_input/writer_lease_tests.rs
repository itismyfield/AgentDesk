//! Every legacy tmux content writer against an owner-held target: a thread without the owner's
//! scope and a write to another target fail before any byte; unheld targets write as main.
use crate::services::claude_tui::host_input::InputTransport;
use crate::services::claude_tui::owner_input;
use crate::services::session_host::{HostSessionRef, InteractiveSessionHost, TmuxHost};
use crate::services::tui_input::submission_tmux::SubmissionTmux;
use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

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

fn owner_scope<R>(owner: &str, commits: Arc<AtomicUsize>, run: impl FnOnce() -> R) -> R {
    let root = crate::services::tmux_common::current_tmux_owner_marker();
    let nonce = format!("{:0>32}", owner.len());
    let context = serde_json::json!({"schema":1,"provider":"claude","created_at":chrono::Utc::now(),"execution_nonce":&nonce,"tmux_session":owner,"channel_id":742702,"owner_runtime_root":&root,"host":"lease-test","expected_native_session_id":null,"launch_mode":"fresh","provider_root":null});
    let _ = crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(
        serde_json::from_value(context).unwrap(),
    );
    let marker = crate::services::tmux_common::session_temp_path(owner, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, &nonce).unwrap();
    owner_input::with_scope(
        "claude".into(),
        serde_json::json!({"host":"tmux","owner":owner,"root":root,"execution_nonce":nonce}),
        742702,
        Arc::new(move |_| {
            commits.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        Arc::new(|_, _| Ok(())),
        Arc::new(|_| Ok(())),
        run,
    )
}

fn received(f: &Fixture, name: &str) -> String {
    std::thread::sleep(std::time::Duration::from_millis(200));
    std::fs::read_to_string(f.root.path().join(name).join("received")).unwrap_or_default()
}

/// An owner writes its held target through its start commit; a spawned thread or another target
/// is refused unsent, and once the owner ends both run as main.
#[test]
fn held_tmux_target_refuses_unscoped_and_foreign_content_at_every_writer() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    f.session("a4b-held");
    f.session("a4b-free");
    let commits = Arc::new(AtomicUsize::new(0));
    owner_scope("a4b-held", commits.clone(), || {
        for (name, write) in WRITERS {
            assert_eq!(write("a4b-held"), Ok(()), "{name}: owner target");
            assert!(write("a4b-free").is_err(), "{name}: foreign target");
            let spawned = std::thread::spawn(move || write("a4b-held"));
            assert!(
                spawned.join().unwrap().is_err(),
                "{name}: thread lacks scope"
            );
        }
    });
    assert_eq!(commits.load(Ordering::SeqCst), WRITERS.len());
    let held = received(&f, "a4b-held");
    assert_eq!(received(&f, "a4b-free"), "");
    for (name, write) in WRITERS {
        assert_eq!(write("a4b-free"), Ok(()), "{name}: unheld target");
        let spawned = std::thread::spawn(move || write("a4b-held"));
        assert_eq!(spawned.join().unwrap(), Ok(()), "{name}: released target");
    }
    let free = received(&f, "a4b-free");
    for text in ["lit", "sub", "leg", "pas", "host"] {
        assert_eq!(held.matches(text).count(), 1, "{text} once in {held:?}");
        assert!(free.contains(text), "{text} in {free:?}");
    }
}

/// A Codex argv launch for an owner-held session from a thread without the owner's scope never
/// creates the session.
#[test]
fn held_tmux_target_refuses_an_unscoped_argv_launch() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let context: crate::services::tui_prompt_dedupe::binding_context::BindingContext =
        serde_json::from_value(serde_json::json!({"schema":1,"provider":"codex","created_at":chrono::Utc::now(),"execution_nonce":"a4b","tmux_session":"a4b-argv","channel_id":742702,"owner_runtime_root":"/a4b","host":"argv-test","expected_native_session_id":null,"launch_mode":"fresh","provider_root":null})).unwrap();
    let context = Arc::new(context);
    owner_scope("a4b-argv", Arc::default(), || {
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
