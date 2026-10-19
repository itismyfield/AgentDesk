//! Node-local card preferences; published replacements immediately update every channel reader.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::durable;

const DIR_NAME: &str = "o_tool_card_switch";
const MAX_BYTES: u64 = 4 << 10;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Override {
    schema: u32,
    pub enabled: bool,
    pub by: u64,
    pub at: DateTime<Utc>,
}

pub(crate) type Value = Result<Option<Override>, String>;
type Versions = HashMap<(PathBuf, u64), watch::Sender<Value>>;
static VERSIONS: LazyLock<Mutex<Versions>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static WRITE: Mutex<()> = Mutex::new(());

/// An error before publication changes nothing; a rename followed by a failed sync is distinct.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Published {
    Durable,
    PublishedUnsynced(String),
}

fn path(root: &Path, channel: u64) -> PathBuf {
    root.join(DIR_NAME).join(format!("{channel}.json"))
}

fn read(root: &Path, channel: u64) -> Value {
    let file = match fs::File::open(path(root, channel)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("tool card preference exceeds its size limit".into());
    }
    let value: Override = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if value.schema != 1 {
        return Err(format!("tool card preference schema {}", value.schema));
    }
    Ok(Some(value))
}

fn sender(root: &Path, channel: u64) -> watch::Sender<Value> {
    let mut versions = VERSIONS.lock().unwrap_or_else(PoisonError::into_inner);
    versions
        .entry((root.to_path_buf(), channel))
        .or_insert_with(|| {
            let value = read(root, channel);
            if let Err(error) = &value {
                tracing::warn!(channel, %error, "tool card preference unreadable; cards disabled");
            }
            watch::channel(value).0
        })
        .clone()
}

pub(crate) fn status(root: &Path, channel: u64) -> Value {
    sender(root, channel).borrow().clone()
}

pub(crate) fn global_enabled() -> bool {
    crate::config_live_reload::current()
        .and_then(|config| config.tui_o.as_ref().map(|o| o.tool_cards.enabled))
        .unwrap_or(true)
}

pub(crate) fn effective(global: bool, value: &Value) -> bool {
    global
        && value
            .as_ref()
            .is_ok_and(|v| v.as_ref().is_none_or(|v| v.enabled))
}

pub(crate) struct ToolCardSwitch;

impl ToolCardSwitch {
    /// Only constructing a reader performs file I/O; each decision reads live config and watch.
    pub(crate) fn for_channel(channel: u64) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let Some(root) = crate::config::runtime_root() else {
            tracing::warn!(
                channel,
                "tool card runtime root unavailable; cards disabled"
            );
            return Arc::new(|| false);
        };
        Self::for_root(&root, channel, Arc::new(global_enabled))
    }

    fn for_root(
        root: &Path,
        channel: u64,
        global: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let value = sender(root, channel).subscribe();
        Arc::new(move || effective(global(), &value.borrow()))
    }
}

pub(crate) fn set(root: &Path, channel: u64, enabled: bool, by: u64) -> Result<Published, String> {
    let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    let value = Override {
        schema: 1,
        enabled,
        by,
        at: Utc::now(),
    };
    let bytes = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    let dir = root.join(DIR_NAME);
    durable::ensure_dir(&dir).map_err(|error| error.to_string())?;
    durable::sweep_tmp(&dir).map_err(|error| error.to_string())?;
    let target = path(root, channel);
    let published = match durable::replace(&target, &bytes) {
        Ok(()) => Published::Durable,
        Err(error) if fs::read(&target).is_ok_and(|now| now == bytes) => {
            Published::PublishedUnsynced(error.to_string())
        }
        Err(error) => return Err(error.to_string()),
    };
    let _ = sender(root, channel).send_replace(Ok(Some(value)));
    Ok(published)
}

#[cfg(test)]
#[path = "tool_card_switch_tests.rs"]
mod tests;
