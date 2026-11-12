use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use super::tests::{append_record, end, history, prompt, record, tool};
use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use serde_json::json;

struct Fixture {
    _root: tempfile::TempDir,
    binding: SourceBinding,
    end: RestartEnd,
    opener: CapturedRecord,
    last: CapturedRecord,
    next: CapturedRecord,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let opener = prompt(0, "old-open");
        let last = tool(opener.end);
        let next = prompt(last.end, "new-open");
        let mut bytes = Vec::new();
        for row in [&opener, &last, &next] {
            append_record(&mut bytes, row);
        }
        let path = root.path().join("native.jsonl");
        std::fs::write(&path, &bytes).unwrap();
        let binding = SourceBinding {
            channel_id: 810_923_101,
            provider: ShadowProvider::Claude,
            source: source_id_for("repair-session", &path).unwrap(),
        };
        let end = end(&binding, &opener, &last, &bytes);
        Self {
            _root: root,
            binding,
            end,
            opener,
            last,
            next,
        }
    }

    fn verified(&self) -> VerifiedEnd {
        validated(vec![self.end.clone()], &history(&self.end), &self.binding)
            .unwrap()
            .pop()
            .expect("the exact original end must validate")
    }

    fn opened(&self, ends: Vec<VerifiedEnd>) -> Projection {
        let mut projection = Projection {
            ends,
            ..Projection::default()
        };
        assert!(projection.before(&self.binding, &self.opener).is_none());
        assert!(projection.before(&self.binding, &self.last).is_none());
        projection
    }
}

#[test]
fn anonymous_codex_native_close_cannot_be_closed_again_by_a_physical_end() {
    let root = tempfile::tempdir().unwrap();
    let opener = record(
        0,
        json!({"type":"event_msg","payload":{"type":"task_started"}}),
    );
    let closer = record(
        opener.end,
        json!({"type":"event_msg","payload":{"type":"task_complete"}}),
    );
    let mut bytes = Vec::new();
    append_record(&mut bytes, &opener);
    append_record(&mut bytes, &closer);
    let path = root.path().join("anonymous.jsonl");
    std::fs::write(&path, &bytes).unwrap();
    let binding = SourceBinding {
        channel_id: 810_923_102,
        provider: ShadowProvider::Codex,
        source: source_id_for("anonymous-session", &path).unwrap(),
    };
    let mut end = end(&binding, &opener, &closer, &bytes);
    end.native_turn_id = None;
    let mut projection = Projection {
        ends: validated(vec![end.clone()], &history(&end), &binding).unwrap(),
        ..Projection::default()
    };
    assert!(projection.before(&binding, &opener).is_none());
    assert!(projection.before(&binding, &closer).is_none());
    assert!(
        projection.through(&binding.source, closer.end).is_none(),
        "the anonymous native Idle already ended this turn"
    );
    let window = &projection.windows[&source_key(&binding.source)];
    assert!(window.idle && window.open_after.is_none());
    assert!(!projection.applied.contains(&end.end_id));
}

#[test]
fn wrong_opener_id_nonce_or_cutoff_never_releases_writer_keys() {
    let fixture = Fixture::new();
    for change in ["opener", "id", "nonce", "cutoff"] {
        let mut verified = fixture.verified();
        let ends = match change {
            "opener" => {
                verified.end.open_after -= 1;
                vec![verified]
            }
            "id" => {
                verified.end.native_turn_id = Some("foreign-open".into());
                vec![verified]
            }
            "nonce" => {
                let mut wrong = fixture.end.clone();
                wrong.execution_nonce = "b".repeat(32);
                let checked =
                    validated(vec![wrong], &history(&fixture.end), &fixture.binding).unwrap();
                assert!(
                    checked.is_empty(),
                    "a foreign nonce has no original binding scope"
                );
                checked
            }
            "cutoff" => vec![verified],
            _ => unreachable!(),
        };
        let mut projection = fixture.opened(ends);
        let source = &fixture.binding.source;
        let key = source_key(source);
        let before = projection.windows[&key].keys.clone();
        assert!(!before.is_empty());
        let through = fixture.end.cutoff + u64::from(change == "cutoff");
        assert!(projection.through(source, through).is_none(), "{change}");
        let window = &projection.windows[&key];
        assert_eq!(
            window.keys, before,
            "{change} must preserve outstanding units"
        );
        assert_eq!(window.open_after, Some(fixture.opener.end), "{change}");
        assert_eq!(window.native_id.as_deref(), Some("old-open"), "{change}");
        assert!(
            !projection.applied.contains(&fixture.end.end_id),
            "{change}"
        );
    }
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

#[test]
fn scope_rejection_and_a_crossed_cutoff_log_identity_once() {
    let fixture = Fixture::new();
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let mut wrong = fixture.end.clone();
        wrong.execution_nonce = "b".repeat(32);
        let rejected = validated(vec![wrong], &history(&fixture.end), &fixture.binding).unwrap();
        assert!(rejected.is_empty());
        let rejected = logs.text();
        let entries: Vec<_> = rejected
            .lines()
            .filter(|line| line.contains(&fixture.end.end_id))
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "scope rejection must name the rejected end: {rejected}"
        );
        assert!(
            entries[0].contains("WARN")
                && entries[0].contains("end_id=")
                && entries[0].contains("reason=")
        );

        logs.clear();
        let mut projection = fixture.opened(Vec::new());
        assert!(projection.before(&fixture.binding, &fixture.next).is_none());
        projection.ends = vec![fixture.verified()];
        for _ in 0..2 {
            assert!(
                projection
                    .through(&fixture.binding.source, fixture.next.end)
                    .is_none()
            );
        }
        let crossed = logs.text();
        let entries: Vec<_> = crossed
            .lines()
            .filter(|line| line.contains(&fixture.end.end_id))
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "a repeatedly crossed cutoff logs once per end: {crossed}"
        );
        assert!(
            entries[0].contains("WARN")
                && entries[0].contains("end_id=")
                && entries[0].contains("reason=")
        );
    });
}

#[test]
fn suffix_append_during_prefix_verification_preserves_the_exact_cutoff() {
    let fixture = Fixture::new();
    let path = fixture.binding.source.path.clone();
    let original_len = std::fs::metadata(&path).unwrap().len();
    let suffix = b"{\"type\":\"system\"}\n";
    AFTER_HASH.with_borrow_mut(|hook| {
        assert!(hook.is_none());
        *hook = Some(Box::new(move || {
            std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .unwrap()
                .write_all(suffix)
                .unwrap();
        }));
    });

    let snapshot = prefix_matches(&fixture.end)
        .expect("appending beyond the exact cutoff does not invalidate its prefix")
        .expect("the original prefix still has the physical end's hash");
    assert_eq!(snapshot.len, original_len + suffix.len() as u64);
    assert_eq!(
        snapshot,
        Fingerprint::path(&fixture.binding.source).unwrap()
    );
    AFTER_HASH.with_borrow(|hook| assert!(hook.is_none()));
}

#[test]
fn rewrite_and_append_after_hash_cannot_pair_an_old_digest_with_a_new_snapshot() {
    let fixture = Fixture::new();
    let path = fixture.binding.source.path.clone();
    let original = std::fs::read(&path).unwrap();
    let original_snapshot = Fingerprint::path(&fixture.binding.source).unwrap();
    let cutoff = fixture.end.cutoff as usize;
    let at = original
        .windows(4)
        .position(|bytes| bytes == b"work")
        .unwrap();
    assert!(
        at + 4 <= cutoff,
        "the mutation must precede the witnessed cutoff"
    );
    let mut rewritten = original.clone();
    rewritten[at..at + 4].copy_from_slice(b"edit");
    let suffix = b"{\"type\":\"system\"}\n";
    AFTER_HASH.with_borrow_mut(|hook| {
        assert!(hook.is_none());
        *hook = Some(Box::new(move || {
            std::fs::write(&path, rewritten).unwrap();
            std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .unwrap()
                .write_all(suffix)
                .unwrap();
        }));
    });

    let result = prefix_matches(&fixture.end);
    AFTER_HASH.with_borrow(|hook| assert!(hook.is_none()));
    let changed = std::fs::read(&fixture.binding.source.path).unwrap();
    let changed_snapshot = Fingerprint::path(&fixture.binding.source).unwrap();
    assert_eq!(changed.len(), original.len() + suffix.len());
    assert_eq!(
        (original_snapshot.dev, original_snapshot.ino),
        (changed_snapshot.dev, changed_snapshot.ino),
        "the fixture must retain the same native file identity"
    );
    assert_ne!(
        hex::encode(Sha256::digest(&changed[..cutoff])),
        fixture.end.prefix_hash,
        "the native prefix must actually differ after the hash completed"
    );
    assert!(
        !matches!(&result, Ok(Some(_))),
        "a pre-mutation digest must not authenticate the post-mutation snapshot: {result:?}"
    );
}

#[cfg(unix)]
#[test]
fn an_unreadable_native_source_does_not_poison_healthy_ends() {
    use std::os::unix::fs::PermissionsExt;

    struct RestorePermissions(std::path::PathBuf, std::fs::Permissions);
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            std::fs::set_permissions(&self.0, self.1.clone()).unwrap();
        }
    }

    let unreadable = Fixture::new();
    let path = &unreadable.binding.source.path;
    let _restore = RestorePermissions(path.clone(), std::fs::metadata(path).unwrap().permissions());
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        std::fs::read(path).is_err(),
        "the native-source fixture must deny reads"
    );
    let rejected = validated(
        vec![unreadable.end.clone()],
        &history(&unreadable.end),
        &unreadable.binding,
    )
    .expect("one native-source read error must stay local to its end");
    assert!(rejected.is_empty());

    let healthy = Fixture::new();
    let checked = validated(
        vec![healthy.end.clone()],
        &history(&healthy.end),
        &healthy.binding,
    )
    .expect("a separate healthy source remains usable while the bad source is unreadable");
    assert_eq!(checked.len(), 1);
    assert_eq!(checked[0].end.end_id, healthy.end.end_id);
    assert!(checked[0].native_current(&healthy.binding.source));
}
