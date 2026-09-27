use super::ChannelSessionState;
use std::future::Future;

pub(super) async fn load_with<R>(
    fetch: impl Future<Output = Result<Option<R>, sqlx::Error>>,
    read: impl Fn(&R, &str) -> Result<Option<String>, sqlx::Error>,
) -> Result<Option<ChannelSessionState>, String> {
    let Some(row) = fetch
        .await
        .map_err(|error| format!("query channel session: {error}"))?
    else {
        return Ok(None);
    };
    let get = |field| read(&row, field).map_err(|error| format!("decode session {field}: {error}"));
    Ok(Some(ChannelSessionState {
        agent_id: get("agent_id")?,
        provider: get("provider")?,
        status: get("status")?,
        active_dispatch_id: get("active_dispatch_id")?,
        thread_channel_id: get("thread_channel_id")?,
    }))
}

pub(super) async fn enrich_with<F: Future<Output = Result<Option<ChannelSessionState>, String>>>(
    json: &mut serde_json::Value,
    mut lookup: impl FnMut(u64) -> F,
) {
    let Some(mailboxes) = json
        .get_mut("mailboxes")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for mailbox in mailboxes {
        let Some(channel_id) = mailbox
            .get("channel_id")
            .and_then(serde_json::Value::as_u64)
        else {
            continue;
        };
        let result = lookup(channel_id).await;
        // Keep legacy fields compatible; this additive field distinguishes failed measurement.
        mailbox["session_lookup_error"] = match &result {
            Ok(_) => serde_json::Value::Null,
            Err(error) => serde_json::json!({
                "safety_gate": "measurement_unavailable",
                "fix_safety": crate::cli::doctor::contract::FixSafety::NotFixable,
                "message": error,
            }),
        };
        if let Ok(Some(session)) = result {
            let active_dispatch_present = session
                .active_dispatch_id
                .as_deref()
                .is_some_and(|id| !id.trim().is_empty());
            mailbox["session_record_present"] = serde_json::json!(true);
            mailbox["session_agent_id"] = serde_json::json!(session.agent_id);
            mailbox["session_provider"] = serde_json::json!(session.provider);
            mailbox["session_status"] = serde_json::json!(session.status);
            mailbox["session_active_dispatch_id"] = serde_json::json!(session.active_dispatch_id);
            mailbox["session_thread_channel_id"] = serde_json::json!(session.thread_channel_id);
            if active_dispatch_present {
                mailbox["active_dispatch_present"] = serde_json::json!(true);
            }
        } else {
            mailbox["session_record_present"] = serde_json::json!(false);
            mailbox["session_status"] = serde_json::Value::Null;
            mailbox["session_active_dispatch_id"] = serde_json::Value::Null;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn session_lookup_preserves_failures_and_absence_on_health_wire() {
        for case in [
            "query",
            "agent_id",
            "provider",
            "status",
            "active_dispatch_id",
            "thread_channel_id",
            "absent",
            "null",
            "blank",
            "active",
        ] {
            let failed = !matches!(case, "absent" | "null" | "blank" | "active");
            let lookup = load_with(
                async {
                    match case {
                        "query" => Err(sqlx::Error::PoolTimedOut),
                        "absent" => Ok(None),
                        _ => Ok(Some(())),
                    }
                },
                |_, field| {
                    if field == case {
                        return Err(sqlx::Error::ColumnDecode {
                            index: field.into(),
                            source: std::io::Error::other("injected decode failure").into(),
                        });
                    }
                    Ok(match (field, case) {
                        ("active_dispatch_id", "active") => Some(" dispatch-1 ".into()),
                        ("active_dispatch_id", "blank") => Some(" \t ".into()),
                        ("status", _) => Some("working".into()),
                        _ => None,
                    })
                },
            )
            .await;
            assert_eq!(lookup.is_err(), failed, "{case}: {lookup:?}");
            if case == "absent" {
                assert!(lookup.as_ref().unwrap().is_none());
            }
            let mut wire =
                json!({"mailboxes": [{"channel_id": 77, "active_dispatch_present": false}]});
            let mut calls = 0;
            enrich_with(&mut wire, |channel| {
                assert_eq!(channel, 77);
                calls += 1;
                std::future::ready(lookup.clone())
            })
            .await;
            assert_eq!(calls, 1);
            let mailbox = &wire["mailboxes"][0];
            if failed {
                assert_eq!(
                    mailbox["session_lookup_error"]["safety_gate"], "measurement_unavailable",
                    "{case}: {wire}"
                );
                assert_eq!(mailbox["session_lookup_error"]["fix_safety"], "not_fixable");
                assert!(
                    mailbox["session_lookup_error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains(if case == "query" { "query" } else { case })
                );
            } else {
                assert!(mailbox["session_lookup_error"].is_null(), "{case}: {wire}");
                assert_eq!(mailbox["session_record_present"], case != "absent");
                assert_eq!(mailbox["active_dispatch_present"], case == "active");
                if case == "absent" || case == "null" {
                    assert!(mailbox["session_active_dispatch_id"].is_null());
                }
            }
        }
        assert!(
            super::super::load_channel_session_state(None, 77)
                .await
                .is_err()
        );
    }
}
