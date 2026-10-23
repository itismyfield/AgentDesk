//! Startup bindings can leave both native sessions empty; the transcript still has to prove
//! exactly the expected fresh session before its old source can retire.

use super::*;

const OLD_SESSION: &str = "11111111-1111-4111-8111-111111111111";
const NEW_SESSION: &str = "22222222-2222-4222-8222-222222222222";
const OTHER_SESSION: &str = "33333333-3333-4333-8333-333333333333";

fn session_row(session: &str, id: &str) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(&row(id, id)).unwrap();
    value["sessionId"] = session.into();
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    bytes
}

fn started_empty() -> (Harness, PathBuf, SourceId, Arc<FakeBindings>) {
    let body = session_row(OLD_SESSION, "before");
    let mut stored = None;
    let harness = Harness::build(|runtime| {
        let path = runtime.join("t.jsonl");
        std::fs::write(&path, &body).unwrap();
        let source_id = source_id_for("", &path).unwrap();
        stored = Some((path, source_id.clone()));
        vec![InitSource {
            source_id,
            delivery_start: body.len() as u64,
            prefix_hash: hex::encode(Sha256::digest(&body)),
        }]
    });
    let (path, source) = stored.unwrap();
    let bindings = Arc::new(FakeBindings::new());
    let target = BindingTarget::Source(source.clone());
    bindings.commit(bound(1, None, target, BindingCause::Startup, None));
    harness.gate.acquired();
    (harness, path, source, bindings)
}

fn successor_empty(path: &Path, bytes: &[u8]) -> (PathBuf, SourceId) {
    transcript(path, &format!("{NEW_SESSION}.jsonl"), "", bytes)
}

fn bound_empty(
    bindings: &FakeBindings,
    old: &SourceId,
    new: &SourceId,
    expected: &str,
    mode: &str,
) {
    assert!(old.session_id.is_empty() && new.session_id.is_empty());
    let launch = nonce();
    launched(&launch, expected, mode);
    bindings.commit(hop(2, old, new, BindingCause::Startup, &launch));
}

fn empty_engine(harness: &Harness, bindings: Arc<FakeBindings>, dormant: bool) -> ReaderEngine {
    let policy = dormant.then_some(ReaderConfig { dormant: true });
    reader_engine_at(harness, bindings, policy, SystemTime::now())
}

async fn retires_after_quiet_validation(
    harness: &Harness,
    old: &SourceId,
    dormant: bool,
    (mut sources, mut writer, mut deriver, mut owed): ReaderEngine,
) {
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    let now = SystemTime::now();
    tend_validated(&mut sources, &mut writer, now).await;
    assert!(!retired(harness, old), "proof alone does not retire");
    tokio::time::advance(RETIRE_QUIET + Duration::from_secs(1)).await;
    let quiet = now + RETIRE_QUIET + Duration::from_secs(1);
    sources.tend_at(&mut writer, quiet).unwrap();
    if dormant {
        assert!(
            !retired(harness, old) && probe::validating(&sources),
            "quiet starts validation without retiring"
        );
        validated(&sources).await;
        assert!(!retired(harness, old), "validation must be consumed");
        sources.tend_at(&mut writer, quiet).unwrap();
    }
    assert!(
        retired(harness, old),
        "proven and validated old source retires"
    );
}

async fn empty_sessions_retire(dormant: bool) {
    let _contexts = contexts();
    let (harness, path, old, bindings) = started_empty();
    let (_, new) = successor_empty(&path, &session_row(NEW_SESSION, "after"));
    publish(&path, clear_end(&old, "nonce", 1));
    bound_empty(&bindings, &old, &new, NEW_SESSION, "fresh");
    let engine = empty_engine(&harness, bindings, dormant);
    assert_eq!(
        successor(&harness, &old).proof,
        Some(2),
        "empty sessions need a verified first-record fresh-hop proof"
    );
    retires_after_quiet_validation(&harness, &old, dormant, engine).await;
}

#[tokio::test(start_paused = true)]
async fn empty_sessions_prove_and_retire_with_the_legacy_engine() {
    empty_sessions_retire(false).await;
}

#[tokio::test(start_paused = true)]
async fn empty_sessions_prove_and_retire_with_the_dormant_engine() {
    empty_sessions_retire(true).await;
}

#[tokio::test(start_paused = true)]
async fn empty_sessions_never_prove_foreign_or_unprepared_fresh_hops() {
    let _contexts = contexts();
    for dormant in [false, true] {
        for case in [
            "other_session",
            "resume",
            "wrong_expected",
            "empty_expected",
            "unnamed",
        ] {
            let (harness, path, old, bindings) = started_empty();
            let session = if case == "other_session" {
                OTHER_SESSION
            } else {
                NEW_SESSION
            };
            let bytes = if case == "unnamed" {
                row("after", "after")
            } else {
                session_row(session, "after")
            };
            let (_, new) = successor_empty(&path, &bytes);
            let expected = match case {
                "wrong_expected" => OTHER_SESSION,
                "empty_expected" => "",
                _ => NEW_SESSION,
            };
            let mode = if case == "resume" { "resume" } else { "fresh" };
            publish(&path, clear_end(&old, "nonce", 1));
            bound_empty(&bindings, &old, &new, expected, mode);
            let (mut sources, mut writer, ..) = empty_engine(&harness, bindings, dormant);
            sources.follow(&mut writer).unwrap();
            assert_eq!(
                successor(&harness, &old).proof,
                None,
                "{case}, dormant={dormant}: an empty binding or matching filename proves nothing"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn empty_sessions_reject_an_inode_replaced_after_the_successor_bound() {
    let _contexts = contexts();
    for dormant in [false, true] {
        let (harness, path, old, bindings) = started_empty();
        let bytes = session_row(NEW_SESSION, "after");
        let (new_path, new) = successor_empty(&path, &bytes);
        bound_empty(&bindings, &old, &new, NEW_SESSION, "fresh");
        let (mut sources, mut writer, ..) = empty_engine(&harness, bindings, dormant);
        let original = new_path.with_extension("original");
        std::fs::rename(&new_path, &original).unwrap();
        std::fs::write(&new_path, &bytes).unwrap();
        let replacement = source_id_for("", &new_path).unwrap();
        assert_ne!(new.ino, replacement.ino, "fixture replaced the inode");
        publish(&path, clear_end(&old, "nonce", 1));
        sources.follow(&mut writer).unwrap();
        assert_eq!(
            successor(&harness, &old).proof,
            None,
            "same session bytes from another inode cannot prove the bound source; dormant={dormant}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn empty_sessions_retry_an_incomplete_first_record_without_another_bind() {
    let _contexts = contexts();
    for dormant in [false, true] {
        let full = session_row(NEW_SESSION, "after");
        for initial in [0, full.len() / 2, full.len() - 1] {
            let (harness, path, old, bindings) = started_empty();
            let (new_path, new) = successor_empty(&path, &full[..initial]);
            publish(&path, clear_end(&old, "nonce", 1));
            bound_empty(&bindings, &old, &new, NEW_SESSION, "fresh");
            let (mut sources, mut writer, deriver, owed) =
                empty_engine(&harness, bindings, dormant);
            sources.follow(&mut writer).unwrap();
            assert_eq!(
                successor(&harness, &old).proof,
                None,
                "EOF before the first complete line proves nothing ({initial} bytes, dormant={dormant})"
            );
            append(&new_path, &full[initial..]);
            sources.follow(&mut writer).unwrap();
            assert_eq!(
                successor(&harness, &old).proof,
                Some(2),
                "a later poll retries without another bind ({initial} bytes, dormant={dormant})"
            );
            assert_eq!(writer.store().binding_checkpoint().unwrap(), Some(2));
            retires_after_quiet_validation(
                &harness,
                &old,
                dormant,
                (sources, writer, deriver, owed),
            )
            .await;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn empty_sessions_retry_a_temporary_read_failure_without_another_bind() {
    let _contexts = contexts();
    for dormant in [false, true] {
        let (harness, path, old, bindings) = started_empty();
        let (new_path, new) = successor_empty(&path, &session_row(NEW_SESSION, "after"));
        bound_empty(&bindings, &old, &new, NEW_SESSION, "fresh");
        let (mut sources, mut writer, deriver, owed) = empty_engine(&harness, bindings, dormant);
        let original = new_path.with_extension("unreadable");
        std::fs::rename(&new_path, &original).unwrap();
        std::fs::create_dir(&new_path).unwrap();
        publish(&path, clear_end(&old, "nonce", 1));
        sources.follow(&mut writer).unwrap();
        assert_eq!(
            successor(&harness, &old).proof,
            None,
            "a read error proves nothing"
        );
        std::fs::remove_dir(&new_path).unwrap();
        std::fs::rename(&original, &new_path).unwrap();
        sources.follow(&mut writer).unwrap();
        assert_eq!(
            successor(&harness, &old).proof,
            Some(2),
            "restored readability is retried without another bind; dormant={dormant}"
        );
        assert_eq!(writer.store().binding_checkpoint().unwrap(), Some(2));
        retires_after_quiet_validation(&harness, &old, dormant, (sources, writer, deriver, owed))
            .await;
    }
}

#[tokio::test(start_paused = true)]
async fn empty_sessions_revalidate_renumbered_successor_prefixes() {
    let _contexts = contexts();
    for dormant in [false, true] {
        for rewritten in [false, true] {
            let (harness, path, old, bindings) = started_empty();
            let (new_path, new) = successor_empty(&path, &session_row(NEW_SESSION, "after"));
            bound_empty(&bindings, &old, &new, NEW_SESSION, "fresh");
            let (mut sources, mut writer, mut deriver, mut owed) =
                empty_engine(&harness, bindings.clone(), dormant);
            sources
                .capture(&mut writer, &mut deriver, &mut owed)
                .unwrap();
            let through = writer.store().cursor(&new).unwrap().captured_through;
            assert!(
                through > 0,
                "the successor has a durable prefix to revalidate"
            );
            drop((sources, writer, deriver, owed));
            let _renumber =
                crate::services::tui_o::shadow::capture::renumber::shift(&new_path, 1 << 40);
            let (mut sources, mut writer, deriver, owed) =
                empty_engine(&harness, bindings, dormant);
            if rewritten {
                std::fs::write(&new_path, session_row(NEW_SESSION, "other")).unwrap();
                assert_eq!(std::fs::metadata(&new_path).unwrap().len(), through);
            }
            publish(&path, clear_end(&old, "nonce", 1));
            sources.follow(&mut writer).unwrap();
            assert_eq!(
                successor(&harness, &old).proof,
                (!rewritten).then_some(2),
                "renumbering needs the stored prefix unchanged; rewritten={rewritten}, dormant={dormant}"
            );
            if !rewritten {
                retires_after_quiet_validation(
                    &harness,
                    &old,
                    dormant,
                    (sources, writer, deriver, owed),
                )
                .await;
            }
        }
    }
}
