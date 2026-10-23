//! Text stop execution shares native delivery results with slash and mailbox stops.
use super::*;

pub(super) async fn handle(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    data: &Data,
    channel_id: ChannelId,
) -> Result<bool, Error> {
    async {
            let stop_lookup =
                cancel_text_stop_token_mailbox(&data.shared, &data.provider, channel_id, "!stop")
                    .await;
            match stop_lookup {
                CommandStop::Native(stop) => {
                    if stop.should_reply() { let _ = msg.reply(&ctx.http, stop.text()).await; }
                    if stop.finish().await.token.is_some() {
                        super::super::stop::after_native_publish(&data.shared, &data.provider, channel_id, Some("!stop")).await;
                    }
                }
                CommandStop::Observed(sent) => {
                    let _ = msg.reply(&ctx.http, super::super::super::turn_bridge::NativeStop::observed_text(sent)).await;
                }

                CommandStop::Session(stop) => {
                    stop.interrupt("!stop").await;
                }
                CommandStop::Stop(stop) => {
                    let policy = super::super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession;
                    let termination_recorded =
                        stop.stop(policy, "!stop").await.termination_recorded;
                    crate::services::turn_cancel_finalizer::finalize_turn_cancel(
                        crate::services::turn_cancel_finalizer::FinalizeTurnCancelRequest::from_text_stop(
                            data.provider.clone(),
                            channel_id,
                            "!stop",
                            termination_recorded,
                        ),
                    );
                    schedule_text_stop_pending_queue_drain(
                        &data.shared,
                        &data.provider,
                        channel_id,
                        "!stop",
                    );
                }
                CommandStop::AlreadyStopping => {
                    let _ = msg.reply(&ctx.http, super::super::ALREADY_STOPPING_RESPONSE).await;
                }
                CommandStop::HostRefused => {
                    let _ = msg
                        .reply(&ctx.http, super::super::HOST_REFUSED_STOP_RESPONSE)
                        .await;
                }
                CommandStop::NoActiveTurn => {
                    let _ = msg.reply(&ctx.http, super::super::NO_ACTIVE_TURN_RESPONSE).await;
                }
                #[cfg(unix)]
                CommandStop::Herdr(stop) => {
                    let _ = msg.reply(&ctx.http, stop.reply()).await;
                }
            }
            Ok(true)
            }.await
}
