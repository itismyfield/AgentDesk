use super::super::super::prompt_builder::build_system_prompt_with_manifest;
use super::super::super::{DispatchProfile, PromptProfiles, settings};
use super::*;

pub(super) async fn build(
    ctx: Context<'_>,
    provider: &ProviderKind,
    session_id: &str,
) -> Result<(String, usize), Error> {
    let shared = &ctx.data().shared;
    let channel = ctx.channel_id();
    let (name, category) = {
        let core = shared.core.lock().await;
        let session = core.sessions.get(&channel);
        (
            session.and_then(|s| s.channel_name.clone()),
            session.and_then(|s| s.category_name.clone()),
        )
    };
    let path = super::super::current_working_dir(shared, channel)
        .await
        .or_else(|| settings::resolve_workspace(channel, name.as_deref()))
        .ok_or("현재 세션의 작업 디렉터리가 없어요.")?;
    let role_channel = shared
        .dispatch
        .role_overrides
        .get(&channel)
        .map(|c| *c)
        .unwrap_or(channel);
    let role = settings::resolve_role_binding(role_channel, name.as_deref());
    let memory = settings::memory_settings_for_binding(role.as_ref());
    let profile = DispatchProfile::for_turn(
        None,
        settings::resolve_dispatch_profile(channel, name.as_deref()),
    );
    let shared_knowledge =
        if profile == DispatchProfile::Full && matches!(provider, ProviderKind::Claude) {
            super::super::super::runtime_store::shared_agent_knowledge_path()
                .and_then(|path| std::fs::read_to_string(path).ok())
                .filter(|text| !text.trim().is_empty())
                .map(|text| format!("[Shared Agent Knowledge]\n{}", text.trim()))
        } else {
            None
        };
    let context = match name {
        Some(name) => format!(
            "Discord context: channel #{name} (ID: {channel}){}",
            category
                .map(|c| format!(" (category: {c})"))
                .unwrap_or_default()
        ),
        None => format!("Discord context: channel {channel}"),
    };
    let built = build_system_prompt_with_manifest(
        &context,
        &shared.channel_roster(channel, ctx.author().id, &ctx.author().name),
        &path,
        channel,
        role_channel,
        &ctx.data().token,
        role.as_ref(),
        false,
        PromptProfiles::inspection(profile),
        None,
        None,
        shared_knowledge.as_deref(),
        None,
        Some(&memory),
        crate::services::mcp_config::provider_has_memento_mcp(provider),
        matches!(provider, ProviderKind::Claude),
        None,
        None,
        None,
    )
    .with_session_anchors(crate::services::memory::SessionAnchorRequest {
        settings: &memory,
        provider,
        current_path: &path,
        channel_id: channel.get(),
        memory_scope_channel_id: role_channel.get(),
        role_binding: role.as_ref(),
        session_id: Some(session_id),
        fresh: false,
    })
    .await;
    let prompt = if matches!(provider, ProviderKind::Codex) {
        crate::services::codex::compose_codex_developer_instructions(
            Some(&built.system_prompt),
            None,
        )
        .unwrap_or_default()
    } else {
        built.system_prompt
    };
    Ok((prompt, built.anchor_count))
}
