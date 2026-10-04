#[cfg(any(target_os = "macos", target_os = "linux"))]
mod supported {
    use super::super::*;
    use crate::services::tui_input::rows::{Owner, RowState};
    use crate::services::tui_input::transition::{Move, Outcome, handback};

    #[derive(Default)]
    struct Fixture {
        dead: bool,
        outbox: bool,
        draft: bool,
        actor_started: usize,
        legacy: Vec<u64>,
        notices: usize,
    }
    impl Effects for Fixture {
        fn intake_outbox_open(&mut self) -> io::Result<bool> {
            Ok(self.outbox)
        }
        fn provider_alive(&mut self) -> io::Result<bool> {
            Ok(!self.dead)
        }
        fn materialize_bundle(&mut self, _: &Value) -> io::Result<Vec<(String, Vec<u8>)>> {
            Ok(vec![("bundle.txt".into(), b"bundle".to_vec())])
        }
        fn evidence(&mut self, _: u64, _: &Value) -> io::Result<MoveEvidence> {
            Ok(MoveEvidence {
                user_record: false,
                turn_open: false,
                composer: if self.draft {
                    Composer::Draft
                } else {
                    Composer::Empty
                },
            })
        }
        fn enqueue(&mut self, key: u64, _: &Value) -> io::Result<EnqueueOutcome> {
            if self.legacy.contains(&key) {
                return Ok(EnqueueOutcome::AlreadyPreserved);
            }
            self.legacy.push(key);
            Ok(EnqueueOutcome::Persisted)
        }
        fn start_actor(&mut self) -> io::Result<()> {
            self.actor_started += 1;
            Ok(())
        }
        fn notice(&mut self, _: Option<u64>, _: &'static str) -> io::Result<()> {
            self.notices += 1;
            Ok(())
        }
    }
    fn sandbox() -> tempfile::TempDir {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
        fs::create_dir_all(&parent).unwrap();
        tempfile::tempdir_in(parent).unwrap()
    }
    fn item(key: u64) -> Value {
        json!({"author_id": 7, "message_id": key, "text": format!("input {key}"), "channel_id": 9})
    }
    fn save(path: &Path, value: &Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    }
    fn files(root: &Path) -> Files<Fixture> {
        Files::new(root, ProviderKind::Claude, 9, Fixture::default())
    }

    #[test]
    fn marker_only_and_merged_payload_survive_move_and_checkpoint() {
        let root = sandbox();
        let mut merged = item(2);
        merged["source_message_ids"] = json!([1, 2]);
        merged["source_text_segments"] =
            json!([{"message_id":1,"text":"first"},{"message_id":2,"text":"second"}]);
        merged["source_message_queued_generations"] =
            json!([{"message_id":1,"queued_generation":4},{"message_id":2,"queued_generation":5}]);
        merged["reply_context"] = json!("reply");
        save(
            &root
                .path()
                .join("discord_pending_queue/claude/token/9.json"),
            &json!([merged.clone()]),
        );
        save(
            &root
                .path()
                .join("discord_pending_queue/claude/token/9.dispatch"),
            &item(8),
        );
        let mut host = files(root.path());
        let mut movement = Move::prepare(root.path(), 9, &mut host).unwrap();
        assert_eq!(movement.advance(&mut host), Outcome::Ledger);
        let mut ledger = Ledger::open(root.path(), 9).unwrap();
        assert_eq!(
            ledger.rows().unwrap().row(2).unwrap().input["source_text_segments"],
            merged["source_text_segments"]
        );
        assert_eq!(
            ledger.rows().unwrap().row(8).unwrap().state,
            RowState::Received
        );
        ledger.checkpoint_rows().unwrap();
        drop(ledger);
        let rows = Ledger::open(root.path(), 9).unwrap().rows().unwrap();
        for key in [8, 2] {
            assert_eq!(rows.owner(key), Owner::Ledger);
        }
        assert_eq!(
            rows.row(2).unwrap().input["source_text_segments"],
            merged["source_text_segments"]
        );
        host.effects.legacy = vec![99];
        assert_eq!(
            handback(root.path(), 9, &mut host).unwrap(),
            Outcome::Legacy
        );
        assert_eq!(host.effects.legacy, vec![99, 8, 2]);
    }

    #[test]
    fn upload_bytes_are_pinned_and_original_population_reads_are_nonmutating() {
        let root = sandbox();
        let upload = root.path().join("sample.txt");
        fs::write(&upload, b"attachment").unwrap();
        let mut queued = item(8);
        queued["pending_uploads"] = json!([format!(
            "[File uploaded] sample.txt → {} (10 bytes)",
            upload.display()
        )]);
        let path = root
            .path()
            .join("discord_pending_queue/claude/token/9.json");
        save(&path, &json!([queued]));
        let original = fs::read(&path).unwrap();
        let mut host = files(root.path());
        let ledger = Ledger::open(root.path(), 9).unwrap();
        host.collect(&ledger).unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(
            children(&root.path().join("input_ledger/9/blobs"))
                .unwrap()
                .is_empty()
        );
        drop(ledger);
        let mut movement = Move::prepare(root.path(), 9, &mut host).unwrap();
        assert_eq!(movement.advance(&mut host), Outcome::Ledger);
        fs::remove_file(upload).unwrap();
        let ledger = Ledger::open(root.path(), 9).unwrap();
        let rows = ledger.rows().unwrap();
        let pins: Vec<crate::services::tui_input::blob::BlobPin> =
            serde_json::from_value(rows.row(8).unwrap().input["blob_pins"].clone()).unwrap();
        assert_eq!(ledger.read_blob(&pins[0]).unwrap(), b"attachment");
    }

    #[test]
    fn row_restart_after_marker_retirement_preserves_richer_metadata() {
        let root = sandbox();
        let row = InflightTurnState::new(
            ProviderKind::Claude,
            9,
            None,
            7,
            8,
            0,
            "input".into(),
            None,
            Some("fixture".into()),
            None,
            None,
            0,
        );
        let path = root.path().join("discord_inflight/claude/9.json");
        let marker_path = root
            .path()
            .join("discord_pending_queue/claude/token/9.dispatch");
        save(&path, &serde_json::to_value(&row).unwrap());
        let mut marker = item(8);
        marker["text"] = json!("input");
        marker["queued_generation"] = json!(row.born_generation);
        marker["source_message_ids"] = json!(row.source_message_ids);
        marker["source_text_segments"] = json!([{"message_id":8,"text":"input"}]);
        save(&marker_path, &marker);
        let mut host = files(root.path());
        let mut movement = Move::prepare(root.path(), 9, &mut host).unwrap();
        let mut successor = row.clone();
        successor.user_msg_id = 10;
        save(&path, &serde_json::to_value(successor).unwrap());
        assert_eq!(movement.advance(&mut host), Outcome::Held);
        assert!(!marker_path.exists());
        save(&path, &serde_json::to_value(&row).unwrap());
        let mut resumed = Move::prepare(root.path(), 9, &mut host).unwrap();
        assert_eq!(resumed.advance(&mut host), Outcome::Ledger);
        assert_eq!(
            Ledger::open(root.path(), 9)
                .unwrap()
                .rows()
                .unwrap()
                .row(8)
                .unwrap()
                .input["source_text_segments"],
            marker["source_text_segments"]
        );
        assert!(!path.exists());
    }

    #[test]
    fn malformed_population_and_outbox_leave_legacy_while_dead_row_stays_held() {
        for outbox in [false, true] {
            let root = sandbox();
            let path = root
                .path()
                .join("discord_pending_queue/claude/token/9.json");
            save(
                &path,
                &if outbox {
                    json!([item(8)])
                } else {
                    json!("malformed")
                },
            );
            let original = fs::read(&path).unwrap();
            let mut host = files(root.path());
            host.effects.outbox = outbox;
            let mut movement = Move::prepare(root.path(), 9, &mut host).unwrap();
            assert_eq!(movement.advance(&mut host), Outcome::Legacy);
            assert_eq!(fs::read(&path).unwrap(), original);
            assert_eq!(host.effects.actor_started, 0);
        }
        for draft in [false, true] {
            let root = sandbox();
            let row = InflightTurnState::new(
                ProviderKind::Claude,
                9,
                None,
                7,
                8,
                0,
                "input".into(),
                None,
                Some("fixture".into()),
                None,
                None,
                0,
            );
            let path = root.path().join("discord_inflight/claude/9.json");
            save(&path, &serde_json::to_value(row).unwrap());
            let mut host = files(root.path());
            host.effects.dead = true;
            host.effects.draft = draft;
            let mut movement = Move::prepare(root.path(), 9, &mut host).unwrap();
            assert_eq!(
                movement.advance(&mut host),
                if draft {
                    Outcome::Ledger
                } else {
                    Outcome::Held
                }
            );
            assert_eq!(path.exists(), !draft);
            assert!(host.effects.notices > 0);
        }
    }

    #[test]
    fn changed_row_under_lock_never_unlinks_successor() {
        let root = sandbox();
        let row = InflightTurnState::new(
            ProviderKind::Claude,
            9,
            None,
            7,
            8,
            0,
            "input".into(),
            None,
            Some("fixture".into()),
            None,
            None,
            0,
        );
        let path = root.path().join("discord_inflight/claude/9.json");
        save(&path, &serde_json::to_value(&row).unwrap());
        let ledger = Ledger::open(root.path(), 9).unwrap();
        let mut host = files(root.path());
        host.collect(&ledger).unwrap();
        let mut successor = row.clone();
        successor.user_msg_id = 10;
        save(&path, &serde_json::to_value(successor).unwrap());
        assert!(
            host.delete(DeletePhase::Row).is_err(),
            "snapshot mismatch must refuse row deletion"
        );
        assert!(path.exists(), "successor row must remain");
    }
}
