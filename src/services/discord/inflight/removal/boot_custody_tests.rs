//! Boot custody contracts, driven through the boot reaper wrapper and read back
//! from the custody directory on disk.

use super::nondestructive_loader_tests::{CLAUDE, Env, G, STALE, row};
use super::*;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

const PANE: &str = "AgentDesk-claude-custody";
type Episode = (PathBuf, serde_json::Value);

/// An owner-1 ExternalInput row whose turn starts at `offset` in `transcript`.
fn tui_direct_row(channel_id: u64, transcript: &Path, offset: u64) -> InflightTurnState {
    let mut state = row(channel_id, Some(PANE));
    (state.request_owner_user_id, state.turn_source) = (1, TurnSource::ExternalInput);
    state.output_path = Some(transcript.display().to_string());
    state.turn_start_offset = Some(offset);
    state.external_turn_id = Some(format!("turn-{channel_id}"));
    state
}

/// Writes `prior` then `turn` and returns the path and the turn's start offset.
fn transcript(env: &Env, name: &str, prior: &str, turn: &str) -> (PathBuf, u64) {
    let path = env.dir().with_file_name(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, format!("{prior}{turn}")).unwrap();
    (path, prior.len() as u64)
}

/// Writes a pending-start record for `provider` and returns its bytes.
fn pending_start(provider: &str, channel_id: u64, source: &Path, offset: u64) -> Vec<u8> {
    let root = crate::services::discord::runtime_store::tui_direct_pending_start_root().unwrap();
    let record = serde_json::json!({
        "provider": provider, "channel_id": channel_id, "tmux_session_name": PANE,
        "prompt_text": "queued", "anchor_message_id": channel_id + 1,
        "lease_relay_owner": "watcher", "lease_turn_id": "queued-turn", "generation": G,
        "created_at_ms": 1, "observed_at_ms": 1, "captured_source": [source, offset],
    });
    let bytes = serde_json::to_vec_pretty(&record).unwrap();
    fs::create_dir_all(&root).unwrap();
    let name = format!("{provider}_{channel_id}_{}.json", channel_id + 1);
    fs::write(root.join(name), &bytes).unwrap();
    bytes
}

async fn boot() -> BootReapReport {
    reap_inflight_rows_at_boot_with_guard(&BootReapOnce::default(), &CLAUDE).await
}

fn custody_root(env: &Env) -> PathBuf {
    env.dir().with_file_name("discord_custody").join("claude")
}

/// Each preserved episode's directory and manifest, keyed by its channel.
fn episodes(env: &Env) -> BTreeMap<u64, Episode> {
    let dirs = fs::read_dir(custody_root(env))
        .into_iter()
        .flatten()
        .flatten();
    dirs.filter_map(|dir| {
        let manifest = fs::read(dir.path().join("manifest.json")).ok()?;
        let manifest: serde_json::Value = serde_json::from_slice(&manifest).ok()?;
        Some((
            manifest["episode"]["channel_id"].as_u64()?,
            (dir.path(), manifest),
        ))
    })
    .collect()
}

fn entry<'a>(episode: &'a Episode, kind: &str) -> Option<&'a serde_json::Value> {
    let entries = episode.1["entries"].as_array()?;
    entries.iter().find(|entry| entry["kind"] == kind)
}

/// Bytes of the copy the `kind` entry points at, if it made one.
fn copy_of(episode: &Episode, kind: &str) -> Option<Vec<u8>> {
    let copy = entry(episode, kind)?["copy"].as_str()?;
    fs::read(episode.0.join(copy)).ok()
}

/// Every file under the provider's custody root with its bytes.
fn custody_tree(env: &Env) -> BTreeMap<PathBuf, Vec<u8>> {
    let dirs = fs::read_dir(custody_root(env)).unwrap().flatten();
    let files = dirs.flat_map(|dir| fs::read_dir(dir.path()).unwrap().flatten());
    files
        .map(|file| (file.path(), fs::read(file.path()).unwrap()))
        .collect()
}

// Contract: a live-pane TUI-direct row the reaper unlinks leaves its bytes, its
// transcript turn and a manifest in custody first.
#[tokio::test]
async fn reaped_live_pane_tui_direct_row_leaves_a_custody_copy() {
    let env = Env::new();
    set_test_tmux_alive_override(Some(&[PANE]));
    let (out, offset) = transcript(&env, "reaped.jsonl", "{\"prior\":1}\n", "{\"turn\":2}\n");
    let mut state = tui_direct_row(5_997_001, &out, offset);
    state.set_restart_mode(InflightRestartMode::DrainRestart);
    (state.born_generation, state.restart_generation) = (G - 3, Some(G - 2));
    let path = env.seed(&state, 0);
    let bytes = fs::read(&path).unwrap();

    let report = boot().await;
    assert_eq!(
        (report.reaped_stale, path.exists()),
        (1, false),
        "{report:?}"
    );
    let episodes = episodes(&env);
    let episode = episodes
        .get(&5_997_001)
        .expect("custody manifest for the reaped row");
    assert_eq!(copy_of(episode, "row"), Some(bytes));
    assert_eq!(
        copy_of(episode, "transcript"),
        Some(b"{\"turn\":2}\n".to_vec())
    );
    let source = entry(episode, "transcript").unwrap();
    let size = fs::metadata(&out).unwrap().len();
    assert_eq!(
        (source["offset"].as_u64(), source["size"].as_u64()),
        (Some(offset), Some(size))
    );
}

// Contract: every owner-1 ExternalInput row, whatever its relay owner, and every
// pending-start record of the provider keep their transcript turn; other rows keep bytes only.
#[tokio::test]
async fn custody_copies_every_tui_direct_turn_and_only_the_bytes_of_other_rows() {
    let env = Env::new();
    let (out, offset) = transcript(&env, "sbr.jsonl", "old\n", "sbr turn\n");
    let mut session_bound = tui_direct_row(5_997_011, &out, offset);
    session_bound.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    let missing = env.dir().with_file_name("missing.jsonl");
    let unowned = tui_direct_row(5_997_012, &missing, 0);
    let mut managed = row(5_997_013, None);
    managed.output_path = Some(out.display().to_string());
    for state in [&session_bound, &unowned, &managed] {
        env.seed(state, 0);
    }
    let (queued, queued_offset) = transcript(&env, "queued.jsonl", "old\n", "queued turn\n");
    let record = pending_start("claude", 5_997_014, &queued, queued_offset);
    pending_start("codex", 5_997_015, &queued, queued_offset);

    boot().await;
    let episodes = episodes(&env);
    let channels: Vec<u64> = episodes.keys().copied().collect();
    assert_eq!(channels, [5_997_011, 5_997_012, 5_997_013, 5_997_014]);
    let sbr = &episodes[&5_997_011];
    assert_eq!(copy_of(sbr, "transcript"), Some(b"sbr turn\n".to_vec()));
    let unowned = &episodes[&5_997_012];
    let lost = entry(unowned, "transcript").expect("a missing transcript is still recorded");
    assert!(
        lost["copy"].is_null() && lost["error"].is_string(),
        "{lost}"
    );
    assert!(copy_of(unowned, "row").is_some());
    let managed = &episodes[&5_997_013];
    assert!(copy_of(managed, "row").is_some() && entry(managed, "transcript").is_none());
    let pending = &episodes[&5_997_014];
    assert_eq!(copy_of(pending, "pending_start"), Some(record));
    assert_eq!(
        copy_of(pending, "transcript"),
        Some(b"queued turn\n".to_vec())
    );
}

// Contract: a later boot that meets an episode already in custody adds nothing and
// leaves the first copy and its marker untouched.
#[tokio::test]
async fn a_later_boot_does_not_duplicate_an_episode_already_in_custody() {
    let env = Env::new();
    let (out, offset) = transcript(&env, "kept.jsonl", "old\n", "turn\n");
    env.seed(&tui_direct_row(5_997_021, &out, offset), 0);
    assert_eq!(boot().await.kept, 1);
    let first = custody_tree(&env);
    assert!(!first.is_empty());

    let mut appended = fs::OpenOptions::new().append(true).open(&out).unwrap();
    appended.write_all(b"later\n").unwrap();
    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(G + 1));
    assert_eq!(boot().await.kept, 1);
    assert_eq!(custody_tree(&env), first);
}

// Contract: when custody cannot be written, the reaper still retires rows and boot goes on.
#[tokio::test]
async fn an_unwritable_custody_root_does_not_stop_the_reaper() {
    let env = Env::new();
    let path = env.seed(&row(5_997_031, None), STALE);
    let blocker = env.dir().with_file_name("discord_custody");
    fs::write(&blocker, "not a directory").unwrap();

    let report = boot().await;
    assert_eq!(
        (report.reaped_stale, path.exists()),
        (1, false),
        "{report:?}"
    );
    assert!(blocker.is_file());
}

// Contract: a transcript turn over the 64 MiB copy cap is recorded by path, offset
// and head hash instead of being copied.
#[tokio::test]
async fn a_transcript_turn_over_the_copy_cap_is_recorded_not_copied() {
    let env = Env::new();
    let (out, _) = transcript(&env, "huge.jsonl", "", "");
    fs::File::options()
        .write(true)
        .open(&out)
        .unwrap()
        .set_len((64 << 20) + 1)
        .unwrap();
    env.seed(&tui_direct_row(5_997_041, &out, 0), 0);

    boot().await;
    let episodes = episodes(&env);
    let episode = &episodes[&5_997_041];
    let segment = entry(episode, "transcript").unwrap();
    assert_eq!(segment["source"].as_str(), out.to_str());
    assert_eq!(segment["offset"].as_u64(), Some(0));
    assert!(
        segment["head_sha256"]
            .as_str()
            .is_some_and(|hash| hash.len() == 64)
    );
    assert!(segment["copy"].is_null(), "{segment}");
    let files = fs::read_dir(&episode.0).unwrap().count();
    assert_eq!((files, copy_of(episode, "row").is_some()), (2, true));
}
