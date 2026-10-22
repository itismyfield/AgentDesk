use super::*;
use crate::services::tui_o::store::rotation::BoundaryEvidence;
use crate::services::tui_o::writer::pieces::{TOOL_CALL_CARD, ToolCardInputs};
use crate::services::tui_prompt_dedupe::binding_events as p5;

fn tool(session: &str, id: &str) -> Vec<u8> {
    codex_line(
        serde_json::json!({"type":"assistant", "uuid":format!("u-{id}"),
        "sessionId":session, "apiBlockIndex":0,
        "message":{"id":id,"content":[{"type":"tool_use","id":id,
            "name":"Bash","input":{"command":"pwd"}}]}}),
    )
}

fn log_event(seq: u64, old: Option<&SourceId>, new: &SourceId) -> Vec<u8> {
    let event = p5::BindingEvent {
        seq,
        channel_id: CHANNEL,
        provider: "claude".into(),
        tmux_session: "tmux".into(),
        execution_nonce: Some("nonce".into()),
        old: old.cloned(),
        new: p5::BindingTarget::Source(new.clone()),
        cause: if seq == 1 {
            p5::BindingCause::Startup
        } else {
            p5::BindingCause::Unknown
        },
        parent_hint: None,
        evidence: p5::BindingEvidence {
            hook_event: Some("session_start".into()),
            received_at: Utc::now(),
        },
        committed_at: Utc::now(),
    };
    let mut bytes = serde_json::to_vec(&event).unwrap();
    bytes.push(b'\n');
    bytes
}

struct BindingRoot(Option<PathBuf>);

impl Drop for BindingRoot {
    fn drop(&mut self) {
        p5::forget_channel_for_tests(CHANNEL);
        p5::set_test_root(self.0.as_deref());
    }
}

async fn observed_boundary(
    a: &SourceId,
    b: &SourceId,
    cards: bool,
) -> (Boundary, BoundaryEvidence, Vec<String>) {
    let harness = Harness::build(|_| {
        vec![InitSource {
            source_id: a.clone(),
            delivery_start: 0,
            prefix_hash: hex::encode(Sha256::digest(b"")),
        }]
    });
    harness.gate.acquired();
    let _root = BindingRoot(p5::test_root());
    p5::set_test_root(Some(harness._runtime.path()));
    let dir = harness._runtime.path().join(p5::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{CHANNEL}.log")),
        [log_event(1, None, a), log_event(2, Some(a), b)].concat(),
    )
    .unwrap();
    let _notice = p5::subscribe_binding_events(CHANNEL).unwrap();
    let bindings = Arc::new(FakeBindings::new());
    bindings.commit(bound(
        1,
        None,
        BindingTarget::Source(a.clone()),
        BindingCause::Startup,
        None,
    ));
    let mut writer = harness.writer();
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone());
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    if cards {
        deriver = deriver.with_tool_cards(ToolCardInputs {
            known: Default::default(),
            recorded_cards: Default::default(),
            switch: Arc::new(|| true),
        });
    }
    let mut owed = RecordingOwe::default();
    sources
        .resume(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    sources.follow(&mut writer).unwrap();
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    while let Some(work) = owed.derived.pop_front() {
        assert_eq!(writer.deliver(&work).await, Step::Done);
    }
    let open = UnitKey {
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        kind: UnitKind::Tool,
        native_key: "open:0".into(),
    };
    assert_eq!(
        writer.store().ledger().excluded(&open),
        Some(if cards {
            TOOL_CALL_CARD
        } else {
            pieces::TOOL_CALL_PANEL
        })
    );
    assert_eq!(
        harness.port.posts().len(),
        if cards { 2 } else { 1 },
        "last parent tool has no closing body yet"
    );
    bindings.commit(bound(
        2,
        Some(a),
        BindingTarget::Source(b.clone()),
        BindingCause::Unknown,
        None,
    ));
    sources.follow(&mut writer).unwrap();
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    let child_owed = owed
        .records
        .iter()
        .filter(|(source, _, _)| source == b)
        .count();
    assert_eq!(
        child_owed, 2,
        "only the fresh tool and body are owed; inherited tool rows stay masked"
    );
    while let Some(work) = owed.derived.pop_front() {
        assert_eq!(writer.deliver(&work).await, Step::Done);
    }
    let from = writer
        .store()
        .rotation()
        .unwrap()
        .link(b)
        .unwrap()
        .boundary
        .clone();
    let evidence = writer
        .store()
        .ledger()
        .boundary_evidence(b)
        .cloned()
        .expect("automatic proof must be durable");
    drop(writer);
    (from, evidence, harness.port.posts())
}

#[tokio::test]
async fn card_mode_preserves_exact_fork_start_and_boundary_evidence_with_an_open_run() {
    let root = tempfile::tempdir().unwrap();
    let a_path = root.path().join("a.jsonl");
    let inherited_tool = tool("a", "inherited");
    let anchor = row("anchor", "parent body");
    let open = tool("a", "open");
    std::fs::write(&a_path, [inherited_tool, anchor.clone(), open].concat()).unwrap();
    let a = source_id_for("a", &a_path).unwrap();
    let inherited = [tool("b", "inherited"), anchor, tool("b", "open")].concat();
    let from = inherited.len() as u64;
    let (_, b) = transcript(
        &a_path,
        "b.jsonl",
        "b",
        &[
            inherited,
            tool("b", "fresh"),
            row("fresh-body", "child body"),
        ]
        .concat(),
    );
    let (off_from, off_evidence, off_posts) = observed_boundary(&a, &b, false).await;
    let (on_from, on_evidence, on_posts) = observed_boundary(&a, &b, true).await;
    assert_eq!(off_from, Boundary::Owed { from });
    assert_eq!(on_from, off_from);
    assert_eq!(
        on_evidence, off_evidence,
        "all source, binding, hash and cursor proof bytes match"
    );
    assert_eq!(off_posts, ["parent body", "child body"]);
    assert_eq!(
        on_posts
            .iter()
            .filter(|payload| payload.starts_with("-# ▸ "))
            .count(),
        2
    );
    assert_eq!(on_posts.last().unwrap(), "child body");
}
