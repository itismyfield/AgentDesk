use super::*;
use crate::services::tui_o::shadow::capture::file_identity;
use crate::services::tui_o::store::fault::{self, Step};

fn witness(root: &Path, id: &str) -> RestartEnd {
    let source_path = root.join(format!("native-{id}.jsonl"));
    fs::write(&source_path, b"open\nbody\n").unwrap();
    let metadata = fs::metadata(&source_path).unwrap();
    let (dev, ino) = file_identity(&metadata);
    RestartEnd {
        end_id: id.into(),
        channel: 81092,
        provider: ShadowProvider::Claude,
        tmux_session: "fixture-pane".into(),
        execution_nonce: "a".repeat(32),
        binding_seq: 1,
        source: SourceId {
            session_id: "native-session".into(),
            path: source_path,
            dev,
            ino,
        },
        cutoff: metadata.len(),
        prefix_hash: hex::encode(Sha256::digest(b"open\nbody\n")),
        open_after: 5,
        native_turn_id: Some("old-turn".into()),
    }
}

#[test]
fn absent_witnesses_do_not_create_storage() {
    let root = tempfile::tempdir().unwrap();
    assert!(read(root.path(), 81092).unwrap().is_empty());
    assert!(read_durable(root.path(), 81092).unwrap().is_empty());
    assert!(!root.path().join(DIR_NAME).exists());
}

#[test]
fn immutable_retry_and_boot_preserve_one_end_after_source_grows() {
    let root = tempfile::tempdir().unwrap();
    let witness = witness(root.path(), "end");
    let version = subscribe(witness.channel);
    let before = *version.borrow();
    assert_eq!(append(root.path(), &witness), Ok(Appended::Durable));
    assert!(*version.borrow() > before);
    fs::write(&witness.source.path, b"open\nbody\nnew-open\n").unwrap();
    assert_eq!(append(root.path(), &witness), Ok(Appended::Durable));
    let restored = read_durable(root.path(), witness.channel).unwrap();
    assert_eq!(restored.as_slice(), std::slice::from_ref(&witness));
    assert_eq!(read(root.path(), witness.channel).unwrap(), restored);
    fs::remove_file(&witness.source.path).unwrap();
    assert_eq!(
        read_durable(root.path(), witness.channel).unwrap(),
        restored
    );
}

#[test]
fn changed_identity_never_overwrites_an_existing_end() {
    let root = tempfile::tempdir().unwrap();
    let witness = witness(root.path(), "end");
    append(root.path(), &witness).unwrap();
    let target = path(root.path(), &witness);
    let original = fs::read(&target).unwrap();
    for change in ["nonce", "cutoff", "seq", "opener", "turn", "source"] {
        let mut changed = witness.clone();
        match change {
            "nonce" => changed.execution_nonce = "b".repeat(32),
            "cutoff" => changed.cutoff += 1,
            "seq" => changed.binding_seq += 1,
            "opener" => changed.open_after -= 1,
            "turn" => changed.native_turn_id = Some("new-turn".into()),
            "source" => changed.source.ino += 1,
            _ => unreachable!(),
        }
        assert!(append(root.path(), &changed).is_err(), "{change}");
        assert_eq!(fs::read(&target).unwrap(), original);
    }
}

#[test]
fn wrong_source_or_eof_publishes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let original = witness(root.path(), "end");
    for change in ["ino", "dev", "cutoff", "hash", "nonce"] {
        let mut changed = original.clone();
        match change {
            "ino" => changed.source.ino += 1,
            "dev" => changed.source.dev += 1,
            "cutoff" => {
                changed.cutoff = 5;
                changed.prefix_hash = hex::encode(Sha256::digest(b"open\n"));
            }
            "hash" => changed.prefix_hash = "0".repeat(64),
            "nonce" => changed.execution_nonce = "not-a-nonce".into(),
            _ => unreachable!(),
        }
        assert!(append(root.path(), &changed).is_err(), "{change}");
        assert!(read(root.path(), original.channel).unwrap().is_empty());
    }
    assert!(!root.path().join(DIR_NAME).exists());
}

#[test]
fn a_torn_eof_is_not_a_physical_turn_boundary() {
    let root = tempfile::tempdir().unwrap();
    let mut witness = witness(root.path(), "end");
    fs::write(&witness.source.path, b"open\nbody").unwrap();
    witness.cutoff = 9;
    witness.prefix_hash = hex::encode(Sha256::digest(b"open\nbody"));
    assert!(append(root.path(), &witness).is_err());
    assert!(read(root.path(), witness.channel).unwrap().is_empty());
}

#[test]
fn historical_witnesses_are_not_evicted_at_sixteen_records() {
    let root = tempfile::tempdir().unwrap();
    let mut originals = Vec::new();
    for seq in 1..=20 {
        let mut witness = witness(root.path(), &format!("end-{seq}"));
        witness.binding_seq = seq;
        append(root.path(), &witness).unwrap();
        originals.push(witness);
    }
    assert_eq!(read_durable(root.path(), 81092).unwrap(), originals);
}

#[test]
fn old_schema_unknown_fields_and_corruption_are_not_rewritten() {
    let root = tempfile::tempdir().unwrap();
    let witness = witness(root.path(), "end");
    append(root.path(), &witness).unwrap();
    let target = path(root.path(), &witness);
    let original = fs::read(&target).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&original).unwrap();
    let mut invalid = vec![b"{broken".to_vec(), vec![b' '; MAX_BYTES as usize + 1]];
    for change in [
        "schema",
        "channel",
        "unknown",
        "unknown_source",
        "empty_nonce",
    ] {
        let mut raw = value.clone();
        match change {
            "schema" => raw["schema"] = 2.into(),
            "channel" => raw["witness"]["channel"] = 1.into(),
            "unknown" => raw["witness"]["future"] = true.into(),
            "unknown_source" => raw["witness"]["source"]["future"] = true.into(),
            "empty_nonce" => raw["witness"]["execution_nonce"] = "".into(),
            _ => unreachable!(),
        }
        invalid.push(serde_json::to_vec(&raw).unwrap());
    }
    for bytes in invalid {
        fs::write(&target, &bytes).unwrap();
        assert!(read_one(&target, witness.channel).is_err());
        assert!(read(root.path(), witness.channel).unwrap().is_empty());
        assert!(append(root.path(), &witness).is_err());
        assert_eq!(fs::read(&target).unwrap(), bytes);
    }
}

#[test]
fn consumers_retry_directory_durability_before_using_an_end() {
    let root = tempfile::tempdir().unwrap();
    let witness = witness(root.path(), "end");
    append(root.path(), &witness).unwrap();
    let target = path(root.path(), &witness);
    let failure = fault::plant(
        &target,
        Step::ReplaceDirSync,
        io::ErrorKind::StorageFull,
        None,
    );
    assert_eq!(
        read(root.path(), witness.channel).unwrap().as_slice(),
        std::slice::from_ref(&witness)
    );
    assert!(read_durable(root.path(), witness.channel).is_err());
    assert!(append(root.path(), &witness).is_err());
    drop(failure);
    assert_eq!(
        read_durable(root.path(), witness.channel)
            .unwrap()
            .as_slice(),
        std::slice::from_ref(&witness)
    );
    assert_eq!(append(root.path(), &witness), Ok(Appended::Durable));
}

#[test]
fn failed_first_write_can_be_retried_without_an_end_or_duplicate() {
    let root = tempfile::tempdir().unwrap();
    let witness = witness(root.path(), "end");
    let failure = fault::plant(root.path(), Step::Write, io::ErrorKind::StorageFull, None);
    assert!(append(root.path(), &witness).is_err());
    assert!(read(root.path(), witness.channel).unwrap().is_empty());
    drop(failure);
    assert_eq!(append(root.path(), &witness), Ok(Appended::Durable));
    assert_eq!(
        read_durable(root.path(), witness.channel).unwrap(),
        [witness]
    );
}

#[test]
fn foreign_channel_and_wrong_record_filename_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let witness = witness(root.path(), "end");
    append(root.path(), &witness).unwrap();
    let target = path(root.path(), &witness);
    let original = fs::read(&target).unwrap();
    let foreign = dir(root.path(), witness.channel + 1);
    fs::create_dir_all(&foreign).unwrap();
    let foreign_target = foreign.join(name(&witness.end_id));
    fs::copy(&target, &foreign_target).unwrap();
    assert!(read_one(&foreign_target, witness.channel + 1).is_err());
    assert!(read(root.path(), witness.channel + 1).unwrap().is_empty());
    assert_eq!(fs::read(&foreign_target).unwrap(), original);
    let renamed = target.with_file_name("wrong.json");
    fs::rename(&target, &renamed).unwrap();
    assert!(read_one(&renamed, witness.channel).is_err());
    assert!(read(root.path(), witness.channel).unwrap().is_empty());
    assert_eq!(fs::read(&renamed).unwrap(), original);
}

#[test]
fn a_corrupt_record_does_not_hide_a_valid_end() {
    let root = tempfile::tempdir().unwrap();
    let good = witness(root.path(), "good");
    append(root.path(), &good).unwrap();
    let corrupt = dir(root.path(), good.channel).join(".DS_Store");
    let bytes = b"not a restart end";
    fs::write(&corrupt, bytes).unwrap();

    assert_eq!(read(root.path(), good.channel).unwrap(), [good.clone()]);
    assert_eq!(read_durable(root.path(), good.channel).unwrap(), [good]);
    assert_eq!(fs::read(corrupt).unwrap(), bytes);
}

#[cfg(unix)]
#[test]
fn an_unreadable_record_does_not_hide_a_valid_end() {
    use std::os::unix::fs::PermissionsExt;

    struct RestorePermissions(PathBuf, fs::Permissions);
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            fs::set_permissions(&self.0, self.1.clone()).unwrap();
        }
    }

    let root = tempfile::tempdir().unwrap();
    let good = witness(root.path(), "good");
    let unreadable = witness(root.path(), "unreadable");
    append(root.path(), &good).unwrap();
    append(root.path(), &unreadable).unwrap();
    let target = path(root.path(), &unreadable);
    let _restore = RestorePermissions(target.clone(), fs::metadata(&target).unwrap().permissions());
    fs::set_permissions(&target, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        fs::read(&target).is_err(),
        "the fixture must deny record reads"
    );

    assert_eq!(read(root.path(), good.channel).unwrap(), [good.clone()]);
    assert_eq!(read_durable(root.path(), good.channel).unwrap(), [good]);
}

#[test]
fn directory_io_failure_remains_a_read_error() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(DIR_NAME)).unwrap();
    fs::write(dir(root.path(), 81092), b"not a directory").unwrap();

    assert!(read(root.path(), 81092).is_err());
    assert!(read_durable(root.path(), 81092).is_err());
}
