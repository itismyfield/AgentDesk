//! Boot custody contracts, driven through the boot reaper wrapper and read back
//! from the custody directory on disk.

use super::nondestructive_loader_tests::{CLAUDE, Env, G, STALE, row};
use super::*;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

const PANE: &str = "AgentDesk-claude-custody";

/// Truncates the named transcript to a length between the copier's stat and its read.
static CUT_BEFORE_COPY: Mutex<Option<(PathBuf, u64)>> = Mutex::new(None);

pub(super) fn before_segment_copy(source: &Path) {
    let cut = CUT_BEFORE_COPY.lock().unwrap().clone();
    if let Some((path, len)) = cut.filter(|(path, _)| path == source) {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_len(len).unwrap();
    }
}

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
fn pending_start(provider: &str, channel_id: u64, source: Option<(&Path, u64)>) -> Vec<u8> {
    let root = crate::services::discord::runtime_store::tui_direct_pending_start_root().unwrap();
    let record = serde_json::json!({
        "provider": provider, "channel_id": channel_id, "tmux_session_name": PANE,
        "prompt_text": "queued", "anchor_message_id": channel_id + 1,
        "lease_relay_owner": "watcher", "lease_turn_id": format!("turn-{channel_id}"),
        "generation": G, "created_at_ms": 1, "observed_at_ms": 1, "captured_source": source,
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

/// Every file under `dir`, recursively, with its bytes.
fn tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(tree(&path));
        } else {
            files.insert(path.clone(), fs::read(&path).unwrap());
        }
    }
    files
}

/// Each episode directory, keyed by the channel its marker names.
fn episodes(env: &Env) -> BTreeMap<u64, PathBuf> {
    let dirs = fs::read_dir(custody_root(env))
        .into_iter()
        .flatten()
        .flatten();
    let channel = |dir: &Path| {
        let marker = ["episode.json", "manifest.json"].map(|name| fs::read(dir.join(name)));
        let marker: serde_json::Value =
            serde_json::from_slice(marker.iter().flatten().next()?).ok()?;
        marker["episode"]["channel_id"].as_u64()
    };
    dirs.filter_map(|dir| Some((channel(&dir.path())?, dir.path())))
        .collect()
}

/// Every manifest entry of an episode with the directory its copy lives in, oldest first.
fn entries(episode: &Path) -> Vec<(PathBuf, serde_json::Value)> {
    let manifests = tree(episode).into_iter();
    let manifests = manifests.filter(|(path, _)| path.ends_with("manifest.json"));
    manifests
        .flat_map(|(path, bytes)| {
            let manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let dir = path.parent().unwrap().to_path_buf();
            let entries = manifest["entries"].as_array().cloned().unwrap_or_default();
            entries.into_iter().map(move |entry| (dir.clone(), entry))
        })
        .collect()
}

fn entry(episode: &Path, kind: &str) -> Option<serde_json::Value> {
    let entries = entries(episode).into_iter();
    entries
        .map(|(_, entry)| entry)
        .find(|entry| entry["kind"] == kind)
}

/// Bytes of every copy the `kind` entries made, concatenated oldest first.
fn copy_of(episode: &Path, kind: &str) -> Option<Vec<u8>> {
    let copies = entries(episode)
        .into_iter()
        .filter(|(_, entry)| entry["kind"] == kind);
    let copies: Vec<Vec<u8>> = copies
        .filter_map(|(dir, entry)| fs::read(dir.join(entry["copy"].as_str()?)).ok())
        .collect();
    (!copies.is_empty()).then(|| copies.concat())
}

fn append(path: &Path, bytes: &str) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes.as_bytes()).unwrap();
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
        .expect("custody marker for the reaped row");
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
    let record = pending_start("claude", 5_997_014, Some((&queued, queued_offset)));
    pending_start("codex", 5_997_015, Some((&queued, queued_offset)));

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

// Contract: a boot that finds nothing new adds nothing, and output appended after an
// earlier copy is preserved before a later boot reaps the row.
#[tokio::test]
async fn a_later_boot_preserves_output_appended_since_the_last_copy() {
    let env = Env::new();
    set_test_tmux_alive_override(Some(&[PANE]));
    let (out, offset) = transcript(&env, "grow.jsonl", "old\n", "turn\n");
    let mut state = tui_direct_row(5_997_021, &out, offset);
    state.set_restart_mode(InflightRestartMode::DrainRestart);
    (state.born_generation, state.restart_generation) = (G - 3, Some(G - 1));
    let path = env.seed(&state, 0);
    assert_eq!(boot().await.kept, 1);
    let first = tree(&custody_root(&env));
    assert!(!first.is_empty());
    assert_eq!(boot().await.kept, 1);
    assert_eq!(tree(&custody_root(&env)), first);

    append(&out, "later\n");
    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(G + 1));
    let report = boot().await;
    assert_eq!(
        (report.reaped_stale, path.exists()),
        (1, false),
        "{report:?}"
    );
    let episodes = episodes(&env);
    assert_eq!(episodes.len(), 1);
    assert_eq!(
        copy_of(&episodes[&5_997_021], "transcript"),
        Some(b"turn\nlater\n".to_vec())
    );
}

// Contract: a pending-start record without a captured source and the row later claimed
// from it are one episode.
#[tokio::test]
async fn a_pending_start_and_the_row_claimed_from_it_are_one_episode() {
    let env = Env::new();
    let (out, offset) = transcript(&env, "claimed.jsonl", "old\n", "turn\n");
    let record = pending_start("claude", 5_997_031, None);
    boot().await;

    let root = crate::services::discord::runtime_store::tui_direct_pending_start_root().unwrap();
    fs::remove_dir_all(root).unwrap();
    env.seed(&tui_direct_row(5_997_031, &out, offset), 0);
    boot().await;
    let dirs = fs::read_dir(custody_root(&env)).unwrap().count();
    let episode = &episodes(&env)[&5_997_031];
    assert_eq!(dirs, 1);
    assert_eq!(copy_of(episode, "pending_start"), Some(record));
    assert_eq!(copy_of(episode, "transcript"), Some(b"turn\n".to_vec()));
}

// Contract: a turn whose start offset moves earlier between boots gets the bytes between the
// new and the old offset preserved, though the transcript itself is unchanged.
#[tokio::test]
async fn an_earlier_turn_start_on_an_unchanged_transcript_is_preserved() {
    let env = Env::new();
    let (out, offset) = transcript(&env, "moved.jsonl", "old\nearlier\n", "turn\n");
    let mut state = tui_direct_row(5_997_051, &out, offset);
    env.seed(&state, 0);
    boot().await;
    state.turn_start_offset = Some(4);
    env.seed(&state, 0);
    boot().await;

    let copies = copy_of(&episodes(&env)[&5_997_051], "transcript").unwrap();
    assert!(
        copies.windows(8).any(|bytes| bytes == b"earlier\n"),
        "{copies:?}"
    );
}

// Contract: a pending-start record that no longer parses is filed under the channel and
// anchor its file name carries, and is recorded as not understood instead of complete.
#[tokio::test]
async fn an_unparseable_pending_start_keeps_its_channel_and_is_not_complete() {
    let env = Env::new();
    let root = crate::services::discord::runtime_store::tui_direct_pending_start_root().unwrap();
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("claude_5997081_5997082.json"), b"{torn").unwrap();
    boot().await;

    let episode = &episodes(&env)[&5_997_081];
    let marker: serde_json::Value =
        serde_json::from_slice(&fs::read(episode.join("episode.json")).unwrap()).unwrap();
    assert_eq!(marker["episode"]["anchor_id"], 5_997_082);
    assert_eq!(copy_of(episode, "pending_start"), Some(b"{torn".to_vec()));
    let record = entry(episode, "pending_start").unwrap();
    assert!(record["error"].is_string(), "{record}");
}

// Contract: a recopy the boot copy budget stops is recorded with that error and supersedes
// nothing, so the earlier copy stays the valid one.
#[tokio::test]
async fn a_recopy_past_the_boot_copy_budget_is_recorded_and_supersedes_nothing() {
    let env = Env::new();
    let prior = format!("old\n{}\n", "p".repeat(40));
    let (out, offset) = transcript(&env, "budget.jsonl", &prior, "turn\n");
    let mut state = tui_direct_row(5_997_101, &out, offset);
    for start in [offset, 4] {
        state.turn_start_offset = Some(start);
        env.seed(&state, 0);
        boot().await;
    }
    let stop = &entries(&episodes(&env)[&5_997_101]).pop().unwrap().1;
    assert_eq!(stop["error"], "boot copy budget exhausted");
    assert!(stop["copy"].is_null() && stop["supersedes"].is_null());
}

// Contract: a transcript cut short between the stat and the copy is recorded as an
// incomplete copy, and the next boot copies the turn again instead of treating it as held.
#[tokio::test]
async fn a_transcript_cut_short_during_the_copy_is_recorded_incomplete_and_retried() {
    let env = Env::new();
    let (out, offset) = transcript(&env, "cut.jsonl", "old\n", "turn body\n");
    env.seed(&tui_direct_row(5_997_041, &out, offset), 0);
    *CUT_BEFORE_COPY.lock().unwrap() = Some((out.clone(), offset + 4));
    boot().await;
    *CUT_BEFORE_COPY.lock().unwrap() = None;
    let episode = episodes(&env)[&5_997_041].clone();
    let segment = entry(&episode, "transcript").unwrap();
    assert!(
        segment["copy"].is_null() && segment["error"].is_string(),
        "{segment}"
    );

    fs::write(&out, "old\nturn body\n").unwrap();
    boot().await;
    assert_eq!(
        copy_of(&episode, "transcript"),
        Some(b"turn body\n".to_vec())
    );
}

// Contract: when custody cannot be written, the reaper still retires rows and boot goes on.
#[tokio::test]
async fn an_unwritable_custody_root_does_not_stop_the_reaper() {
    let env = Env::new();
    let path = env.seed(&row(5_997_061, None), STALE);
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
    env.seed(&tui_direct_row(5_997_071, &out, 0), 0);

    boot().await;
    let episode = &episodes(&env)[&5_997_071];
    let segment = entry(episode, "transcript").unwrap();
    assert_eq!(segment["source"].as_str(), out.to_str());
    assert_eq!(segment["offset"].as_u64(), Some(0));
    assert!(
        segment["head_sha256"]
            .as_str()
            .is_some_and(|hash| hash.len() == 64)
    );
    assert!(segment["copy"].is_null(), "{segment}");
    let parts = tree(episode)
        .into_keys()
        .filter(|path| path.extension() == Some("part".as_ref()));
    assert_eq!(
        (parts.count(), copy_of(episode, "row").is_some()),
        (0, true)
    );
}
