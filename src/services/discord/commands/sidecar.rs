use poise::CreateReply;

use super::super::sidecar_interaction::{
    SIDECAR_DEFAULT_MAC, build_sidecar_components, list_sidecar_devices_on,
    remember_sidecar_pending,
};
use super::super::{Context, Error, check_auth};

/// /sidecar — iPad Sidecar 연결/해제 (호스트 Mac·기기를 드롭다운으로 선택)
#[poise::command(slash_command, rename = "sidecar")]
pub(in crate::services::discord) async fn cmd_sidecar(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /sidecar");

    // Start with the host Mac pre-selected and its device list; selecting a
    // different Mac re-queries that Mac and re-renders the device dropdown.
    let devices = list_sidecar_devices_on(SIDECAR_DEFAULT_MAC).await;
    let components = build_sidecar_components(&devices, Some(SIDECAR_DEFAULT_MAC), None);

    let posted = ctx
        .send(
            CreateReply::default()
                .ephemeral(true)
                .content(
                    "**Sidecar 연결**\n호스트 Mac을 고르면 기기 목록이 그 Mac 기준으로 갱신됩니다. 기기 선택 후 `연결`(또는 `해제`)을 누르세요.",
                )
                .components(components),
        )
        .await?
        .into_message()
        .await?;

    remember_sidecar_pending(posted.id, user_id, Some(SIDECAR_DEFAULT_MAC.to_string()));
    Ok(())
}
