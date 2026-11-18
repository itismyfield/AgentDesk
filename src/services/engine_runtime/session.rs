//! Owned session values used by messenger adapters.

use std::collections::HashMap;

use poise::serenity_prelude::ChannelId;

use crate::services::cluster::attachment_transfer::uploads::PendingUploads;
use crate::ui::ai_screen::HistoryItem;

/// Session values that require one atomic lock.
pub(crate) struct CoreState {
    pub(in crate::services) sessions: HashMap<ChannelId, EngineSession>,
}

pub(crate) struct EngineSession {
    pub(in crate::services) session_id: Option<String>,
    pub(in crate::services) memento_context_loaded: bool,
    pub(in crate::services) memento_reflected: bool,
    pub(in crate::services) current_path: Option<String>,
    pub(in crate::services) history: Vec<HistoryItem>,
    pub(in crate::services) pending_uploads: PendingUploads,
    pub(in crate::services) cleared: bool,
    pub(in crate::services) channel_id: Option<u64>,
    pub(in crate::services) channel_name: Option<String>,
    pub(in crate::services) category_name: Option<String>,
    pub(in crate::services) last_active: tokio::time::Instant,
    pub(in crate::services) worktree: Option<WorktreeInfo>,
    pub(in crate::services) born_generation: u64,
}

impl Clone for EngineSession {
    fn clone(&self) -> Self {
        Self {
            session_id: self.session_id.clone(),
            memento_context_loaded: self.memento_context_loaded,
            memento_reflected: self.memento_reflected,
            current_path: self.current_path.clone(),
            history: self.history.clone(),
            pending_uploads: self.pending_uploads.clone(),
            cleared: self.cleared,
            channel_id: self.channel_id,
            channel_name: self.channel_name.clone(),
            category_name: self.category_name.clone(),
            last_active: self.last_active,
            worktree: self.worktree.clone(),
            born_generation: self.born_generation,
        }
    }
}

/// Paths and branch identity of a session's managed worktree.
#[derive(Clone, Debug)]
pub(crate) struct WorktreeInfo {
    pub(in crate::services) original_path: String,
    pub(in crate::services) worktree_path: String,
    pub(in crate::services) branch_name: String,
}
