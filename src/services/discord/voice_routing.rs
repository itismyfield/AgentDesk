//! Runtime voice-channel to Discord text-channel routing state.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use poise::serenity_prelude::ChannelId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredVoicePairings {
    pairings: BTreeMap<String, String>,
}

#[derive(Clone)]
pub(in crate::services::discord) struct VoiceChannelPairingStore {
    path: Option<PathBuf>,
    pairings: Arc<dashmap::DashMap<u64, u64>>,
}

impl VoiceChannelPairingStore {
    pub(in crate::services::discord) fn load_default() -> Self {
        let path = default_voice_pairings_path();
        let store = Self {
            path,
            pairings: Arc::new(dashmap::DashMap::new()),
        };
        store.load_from_disk();
        store
    }

    #[cfg(test)]
    pub(in crate::services::discord) fn new_for_path(path: PathBuf) -> Self {
        let store = Self {
            path: Some(path),
            pairings: Arc::new(dashmap::DashMap::new()),
        };
        store.load_from_disk();
        store
    }

    pub(in crate::services::discord) fn target_channel(
        &self,
        voice_channel_id: ChannelId,
    ) -> Option<ChannelId> {
        self.pairings
            .get(&voice_channel_id.get())
            .map(|value| ChannelId::new(*value.value()))
    }

    pub(in crate::services::discord) fn attach(
        &self,
        voice_channel_id: ChannelId,
        text_channel_id: ChannelId,
    ) -> Result<(), String> {
        // F13 (#2046): in-memory 변경을 먼저 적용하고 persist 가 실패하면
        // 이전 값(또는 부재 상태)로 되돌려 디스크/메모리 일관성을 유지한다.
        let previous = self
            .pairings
            .insert(voice_channel_id.get(), text_channel_id.get());
        if let Err(error) = self.persist() {
            match previous {
                Some(prev) => {
                    self.pairings.insert(voice_channel_id.get(), prev);
                }
                None => {
                    self.pairings.remove(&voice_channel_id.get());
                }
            }
            return Err(error);
        }
        Ok(())
    }

    pub(in crate::services::discord) fn detach(
        &self,
        voice_channel_id: ChannelId,
    ) -> Result<bool, String> {
        // F13 (#2046): remove 후 persist 실패 시 in-memory 만 비고 디스크엔
        // 남아 재시작 후 살아나는 불일치 회피. 실패 시 이전 값을 복구.
        let previous = self.pairings.remove(&voice_channel_id.get());
        if let Err(error) = self.persist() {
            if let Some((key, value)) = previous {
                self.pairings.insert(key, value);
            }
            return Err(error);
        }
        Ok(previous.is_some())
    }

    fn load_from_disk(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let Ok(raw) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(stored) = serde_json::from_str::<StoredVoicePairings>(&raw) else {
            tracing::warn!(path = %path.display(), "failed to parse voice channel pairings");
            return;
        };
        for (voice_channel_id, text_channel_id) in stored.pairings {
            let Ok(voice_channel_id) = voice_channel_id.parse::<u64>() else {
                continue;
            };
            let Ok(text_channel_id) = text_channel_id.parse::<u64>() else {
                continue;
            };
            self.pairings.insert(voice_channel_id, text_channel_id);
        }
    }

    fn persist(&self) -> Result<(), String> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        let pairings = self
            .pairings
            .iter()
            .map(|entry| (entry.key().to_string(), entry.value().to_string()))
            .collect::<BTreeMap<_, _>>();
        let stored = StoredVoicePairings { pairings };
        let json = serde_json::to_string_pretty(&stored)
            .map_err(|error| format!("serialize voice pairings: {error}"))?;
        super::runtime_store::atomic_write(path, &json)
    }
}

fn default_voice_pairings_path() -> Option<PathBuf> {
    crate::config::runtime_root().map(|root| {
        root.join("runtime")
            .join("discord_voice_channel_pairings.json")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_store_persists_voice_to_text_mapping() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pairings.json");
        let store = VoiceChannelPairingStore::new_for_path(path.clone());

        store
            .attach(ChannelId::new(10), ChannelId::new(20))
            .expect("attach should persist");
        assert_eq!(
            store.target_channel(ChannelId::new(10)),
            Some(ChannelId::new(20))
        );

        let reloaded = VoiceChannelPairingStore::new_for_path(path);
        assert_eq!(
            reloaded.target_channel(ChannelId::new(10)),
            Some(ChannelId::new(20))
        );
    }
}
