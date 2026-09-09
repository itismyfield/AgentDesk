use crate::services::provider::ProviderKind;

use super::checkpoint::CheckpointEvent;
use super::policy::ChannelRecoveryBinding;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryIntake {
    Allow,
    Skip,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FallbackSpawnPlan {
    pub channel_id: String,
    pub owner_agent_id: String,
    pub fallback_agent_id: String,
    pub fallback_provider: ProviderKind,
    pub tmux_name: String,
    pub cwd: String,
    pub watcher_http_bot: String,
    pub prompt: String,
    pub mailbox_handoff_called: bool,
    pub generation: i64,
}

impl FallbackSpawnPlan {
    pub fn from_binding(
        binding: &ChannelRecoveryBinding,
        fallback_provider: ProviderKind,
        events: &[CheckpointEvent],
        generation: i64,
    ) -> Option<Self> {
        let policy = binding.policy.as_ref()?;
        Some(Self {
            channel_id: binding.channel_id.clone(),
            owner_agent_id: binding.owner_agent_id.clone(),
            fallback_agent_id: policy.fallback_agent_id.clone(),
            fallback_provider: fallback_provider.clone(),
            tmux_name: fallback_provider.build_tmux_session_name(&binding.channel_id),
            cwd: binding.workspace.clone(),
            watcher_http_bot: fallback_provider.as_str().to_string(),
            prompt: format_fallback_prompt(binding, events),
            mailbox_handoff_called: false,
            generation,
        })
    }
}

pub fn format_fallback_prompt(
    binding: &ChannelRecoveryBinding,
    events: &[CheckpointEvent],
) -> String {
    let fallback = binding
        .policy
        .as_ref()
        .map(|policy| policy.fallback_agent_id.as_str())
        .unwrap_or("unknown");
    let mut body = format!(
        "[recovery fallback checkpoint log for channel {} owner={} fallback={}]\n",
        binding.channel_id, binding.owner_agent_id, fallback
    );
    for event in events {
        body.push_str(&format!(
            "\n--- seq={} kind={} writer={} ---\n{}\n",
            event.seq,
            event.kind.as_str(),
            event.writer_agent_id,
            event.payload.five_section_text()
        ));
    }
    body.push_str(
        "\nYou are the fallback agent on the original channel. Continue from Next. Do not call mailbox handoff.\n",
    );
    body
}

pub fn effective_handles(yaml_owned: bool, overlay: Option<RecoveryIntake>) -> bool {
    match overlay {
        Some(RecoveryIntake::Skip) => false,
        Some(RecoveryIntake::Allow) => true,
        None => yaml_owned,
    }
}

pub fn dual_processing(
    owner_yaml_owned: bool,
    owner_overlay: Option<RecoveryIntake>,
    fallback_yaml_owned: bool,
    fallback_overlay: Option<RecoveryIntake>,
) -> bool {
    effective_handles(owner_yaml_owned, owner_overlay)
        && effective_handles(fallback_yaml_owned, fallback_overlay)
}
