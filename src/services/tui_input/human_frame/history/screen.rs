//! Composer ownership uses only pending frames in its session, independently of idle headers.
use super::{file_id, path, read};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

#[derive(PartialEq, Eq)]
struct Key {
    path: PathBuf,
    modified: Option<SystemTime>,
    len: u64,
    file: [u64; 2],
}

struct Cached {
    key: Key,
    frames: Vec<(String, String)>,
}

static CACHE: LazyLock<Mutex<Option<Cached>>> = LazyLock::new(|| Mutex::new(None));

fn fold(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

pub(super) fn prefix(frame: &str) -> String {
    let folded = fold(frame);
    let header = folded
        .find(']')
        .map_or(0, |end| folded[..=end].chars().count());
    // Include body bytes even for a long display name, rather than matching an author alone.
    folded.chars().take(256.max(header + 1)).collect()
}

pub(super) fn pending(session: &str, text: &str) -> bool {
    let Ok(path) = path() else { return false };
    let Ok(meta) = std::fs::metadata(&path) else {
        return false;
    };
    let key = Key {
        path,
        modified: meta.modified().ok(),
        len: meta.len(),
        file: file_id(&meta),
    };
    let mut cache = CACHE.lock().unwrap_or_else(|poison| poison.into_inner());
    if cache.as_ref().is_none_or(|cached| cached.key != key) {
        let Some(frames) = frames_for(&key.path) else {
            return false;
        };
        *cache = Some(Cached { key, frames });
    }
    let folded = fold(text);
    cache.as_ref().is_some_and(|cached| {
        cached
            .frames
            .iter()
            .any(|(owner, prefix)| owner == session && folded.contains(prefix))
    })
}

fn frames_for(path: &std::path::Path) -> Option<Vec<(String, String)>> {
    if let Some(frames) = super::compat::screen_frames_v2(path) {
        return Some(frames);
    }
    let store = read(path).ok()?;
    Some(
        store
            .entries
            .into_iter()
            .filter(|entry| {
                entry.provider == "claude"
                    && entry.events.is_empty()
                    && !entry.screen_prefix.is_empty()
            })
            .map(|entry| (entry.session, entry.screen_prefix))
            .collect(),
    )
}
