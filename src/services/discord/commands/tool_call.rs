use std::path::PathBuf;

use poise::CreateReply;

use super::super::{Context, Error, check_auth};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::store::tool_card_switch::{self, Published};

#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
pub(in crate::services::discord) enum ToolCallAction {
    #[name = "on"]
    On,
    #[name = "off"]
    Off,
    #[name = "status"]
    Status,
}

impl ToolCallAction {
    fn enabled(self) -> Option<bool> {
        match self {
            Self::On => Some(true),
            Self::Off => Some(false),
            Self::Status => None,
        }
    }
}

trait CommandContext: Sync {
    fn authorized(&self) -> impl std::future::Future<Output = bool> + Send;
    fn channel(&self) -> u64;
    fn user(&self) -> u64;
    fn kind(&self) -> Option<RuntimeHandoffKind>;
    fn root(&self) -> Option<PathBuf>;
    fn global_enabled(&self) -> bool;
    fn reply(
        &self,
        content: String,
        ephemeral: bool,
    ) -> impl std::future::Future<Output = Result<(), Error>> + Send;
}

fn status_text(global: bool, value: &tool_card_switch::Value, kind: RuntimeHandoffKind) -> String {
    let enabled = tool_card_switch::effective(global, value);
    let basis = if !global {
        "설정 전체 차단"
    } else {
        match value {
            Ok(Some(_)) => "채널 설정",
            Ok(None) => "기본값",
            Err(_) => "설정 파일 오류",
        }
    };
    let provider = match kind {
        RuntimeHandoffKind::ClaudeTui => "Claude",
        RuntimeHandoffKind::CodexTui => "Codex",
        _ => "기타",
    };
    let mut text = format!(
        "도구 카드: {} · 근거: {basis} · 채널: {provider}",
        if enabled { "켬" } else { "끔" }
    );
    if let Ok(Some(value)) = value {
        text.push_str(&format!(
            "\n마지막 변경: <@{}> · {}",
            value.by,
            value.at.to_rfc3339()
        ));
    }
    text
}

async fn run(ctx: &impl CommandContext, action: ToolCallAction) -> Result<(), Error> {
    if !ctx.authorized().await {
        return Ok(());
    }
    let enabled = action.enabled();
    let Some(kind @ (RuntimeHandoffKind::ClaudeTui | RuntimeHandoffKind::CodexTui)) = ctx.kind()
    else {
        return ctx
            .reply(
                "이 채널은 O 출력 채널이 아니라 도구 카드가 나오지 않아요".into(),
                enabled.is_none(),
            )
            .await;
    };
    let Some(root) = ctx.root() else {
        return ctx
            .reply(
                "도구 카드 설정 저장 위치를 확인하지 못했어요".into(),
                enabled.is_none(),
            )
            .await;
    };
    let Some(enabled) = enabled else {
        return ctx
            .reply(
                status_text(
                    ctx.global_enabled(),
                    &tool_card_switch::status(&root, ctx.channel()),
                    kind,
                ),
                true,
            )
            .await;
    };
    let published = match tool_card_switch::set(&root, ctx.channel(), enabled, ctx.user()) {
        Ok(published) => published,
        Err(error) => {
            tracing::warn!(channel_id = ctx.channel(), %error, "tool card preference not published");
            return ctx
                .reply(
                    "도구 카드 설정을 저장하지 못했어요. 설정은 바뀌지 않았어요".into(),
                    false,
                )
                .await;
        }
    };
    let mut text = if enabled {
        "도구 카드: 켬. 다음 도구 묶음부터 적용돼요".to_string()
    } else {
        "도구 카드: 끔. 지금 진행 중인 묶음은 카드로 마무리된 뒤 꺼져요".to_string()
    };
    if !ctx.global_enabled() {
        text.push_str("\n설정 전체 차단이 켜져 있어 현재 실효값은 끔이에요");
    }
    if let Published::PublishedUnsynced(error) = published {
        tracing::warn!(channel_id = ctx.channel(), %error, "tool card preference published without directory sync");
        text.push_str("\n현재 설정은 적용됐지만 디스크 동기화에 실패해 호스트 장애 뒤에는 유지되지 않을 수 있어요");
    }
    ctx.reply(text, false).await
}

struct DiscordContext<'a>(Context<'a>);

impl CommandContext for DiscordContext<'_> {
    async fn authorized(&self) -> bool {
        let ctx = self.0;
        let allowed = check_auth(
            ctx.author().id,
            &ctx.author().name,
            &ctx.data().shared,
            &ctx.data().token,
        )
        .await;
        if allowed {
            log_command_received!(ctx.channel_id().get(), &ctx.author().name, "/tool-call");
        }
        allowed
    }
    fn channel(&self) -> u64 {
        self.0.channel_id().get()
    }
    fn user(&self) -> u64 {
        self.0.author().id.get()
    }
    fn kind(&self) -> Option<RuntimeHandoffKind> {
        crate::services::tui_o::channel_policy::boot().and_then(|boot| boot.kind(self.channel()))
    }
    fn root(&self) -> Option<PathBuf> {
        crate::config::runtime_root()
    }
    fn global_enabled(&self) -> bool {
        tool_card_switch::global_enabled()
    }
    async fn reply(&self, content: String, ephemeral: bool) -> Result<(), Error> {
        if ephemeral {
            self.0
                .send(CreateReply::default().content(content).ephemeral(true))
                .await?;
        } else {
            self.0.say(content).await?;
        }
        Ok(())
    }
}

/// /tool-call on|off|status — 이 채널의 도구 호출 카드를 켜거나 끄거나 상태를 봅니다
#[poise::command(slash_command, rename = "tool-call")]
pub(in crate::services::discord) async fn cmd_tool_call(
    ctx: Context<'_>,
    #[description = "on / off / status"] action: ToolCallAction,
) -> Result<(), Error> {
    run(&DiscordContext(ctx), action).await
}

#[cfg(test)]
#[path = "tool_call_tests.rs"]
mod tests;
