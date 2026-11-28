use std::future::Future;
use std::sync::Arc;

use poise::serenity_prelude::Http;

use crate::domain::bot_role::UtilityBotRole;

pub(in crate::services::discord) async fn resolve_utility_bot_http(
    role: UtilityBotRole,
    lookup: impl Future<Output = Option<Arc<Http>>>,
) -> Result<Arc<Http>, (&'static str, String)> {
    match lookup.await {
        Some(http) => Ok(http),
        None => Err((
            "503 Service Unavailable",
            [
                r#"{"ok":false,"error":""#,
                role.alias(),
                " bot not configured (missing ",
                role.credential_label(),
                r#")"}"#,
            ]
            .concat(),
        )),
    }
}
