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

/// A Herdr owner whose logical session is `logical` on socket `root`; its pins are recorded.
fn herdr_scope<R>(
    logical: &str,
    root: &str,
    pins: Arc<std::sync::Mutex<Vec<String>>>,
    run: impl FnOnce() -> R,
) -> R {
    owner_input::with_scope(
        "claude".into(),
        serde_json::json!({"host":"herdr","owner":logical,"root":root,"execution_nonce":"h1"}),
        742703,
        Arc::new(|_| Ok(())),
        Arc::new(move |_, key| {
            pins.lock().unwrap().push(key);
            Ok(())
        }),
        Arc::new(|_| Ok(())),
        run,
    )
}

fn herdr_write(physical: &str, sent: &AtomicUsize) -> Result<(), String> {
    super::content("herdr", physical, || {
        sent.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
}

/// A Herdr owner writes only the physical pane it bound, never its logical session's name or
/// another pane; it pins that pane, not all Herdr. Unbound, its first write names the pane.
#[test]
fn herdr_owner_writes_only_its_bound_physical_pane() {
    let pins = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sent = AtomicUsize::new(0);
    herdr_scope(
        "AgentDesk-claude-lazy",
        "/sock/lazy",
        Arc::default(),
        || {
            assert_eq!(herdr_write("/sock/lazy#w1-1", &sent), Ok(()));
            assert!(
                herdr_write("/sock/lazy#w1-2", &sent).is_err(),
                "first write bound"
            );
            assert!(owner_input::bind_writer("/sock/lazy#w1-2").is_err());
        },
    );
    herdr_scope(
        "AgentDesk-claude-logical",
        "/sock/only",
        pins.clone(),
        || {
            owner_input::bind_writer("/sock/only#w1-1").unwrap();
            assert!(owner_input::bind_writer("/sock/only#w1-2").is_err());
            assert_eq!(herdr_write("/sock/only#w1-1", &sent), Ok(()));
            assert!(
                herdr_write("/sock/only#w1-2", &sent).is_err(),
                "foreign pane"
            );
            assert!(
                herdr_write("/sock/other#w1-1", &sent).is_err(),
                "foreign socket"
            );
            assert!(
                herdr_write("AgentDesk-claude-logical", &sent).is_err(),
                "the logical name is no pane"
            );
        },
    );
    assert_eq!(sent.load(Ordering::SeqCst), 2);
    assert_eq!(*pins.lock().unwrap(), ["herdr:/sock/only#w1-1"]);
}

/// One physical pane takes one Herdr owner at a time, whatever logical session names it; another
/// pane, or the same pane id on another socket, writes meanwhile.
#[test]
fn herdr_physical_panes_serialize_only_themselves() {
    let (held, release) = (
        Arc::new(std::sync::Barrier::new(2)),
        Arc::new(std::sync::Barrier::new(2)),
    );
    let (h, r) = (held.clone(), release.clone());
    let holder = std::thread::spawn(move || {
        let sent = AtomicUsize::new(0);
        herdr_scope("logical-a", "/sock/a", Arc::default(), || {
            owner_input::bind_writer("/sock/a#w1-1").unwrap();
            let wrote = herdr_write("/sock/a#w1-1", &sent);
            h.wait();
            r.wait();
            wrote
        })
    });
    held.wait();
    let sent = AtomicUsize::new(0);
    let other = |logical: &str, root: &str, physical: &str| {
        herdr_scope(logical, root, Arc::default(), || {
            owner_input::bind_writer(physical).unwrap();
            herdr_write(physical, &sent)
        })
    };
    assert!(
        other("logical-b", "/sock/a", "/sock/a#w1-1").is_err(),
        "same pane"
    );
    assert_eq!(other("logical-c", "/sock/a", "/sock/a#w1-2"), Ok(()));
    assert_eq!(other("logical-d", "/sock/b", "/sock/b#w1-1"), Ok(()));
    release.wait();
    assert_eq!(holder.join().unwrap(), Ok(()));
    assert_eq!(
        other("logical-b", "/sock/a", "/sock/a#w1-1"),
        Ok(()),
        "released"
    );
    assert_eq!(sent.load(Ordering::SeqCst), 3);
}
