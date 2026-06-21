use poise::CreateReply;

use super::super::sidecar_interaction::{
    build_sidecar_components, list_sidecar_devices, remember_sidecar_pending,
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

    let devices = list_sidecar_devices().await;
    let components = build_sidecar_components(&devices);

    let posted = ctx
        .send(
            CreateReply::default()
                .ephemeral(true)
                .content(
                    "**Sidecar 연결**\n호스트 Mac과 기기를 선택한 뒤 `연결`(또는 `해제`)을 누르세요.",
                )
                .components(components),
        )
        .await?
        .into_message()
        .await?;

    remember_sidecar_pending(posted.id, user_id);
    Ok(())
}
