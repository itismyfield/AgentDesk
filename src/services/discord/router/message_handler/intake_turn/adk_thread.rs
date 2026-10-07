use super::*;
use crate::services::discord::adk_session;

pub(super) fn resolve_channel_id(
    adk_session_name: Option<&str>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
) -> Option<u64> {
    adk_session_name
        .and_then(adk_session::parse_thread_channel_id_from_name)
        .or_else(|| {
            shared
                .dispatch
                .thread_parents
                .contains_key(&channel_id)
                .then_some(channel_id.get())
        })
}

pub(super) struct RedirectDispatch<'a> {
    pub http: &'a Arc<serenity::http::Http>,
    pub cache: Option<&'a Arc<serenity::cache::Cache>>,
    pub shared: &'a Arc<SharedData>,
    pub provider: &'a ProviderKind,
    pub channel_id: ChannelId,
    pub original_channel_id: ChannelId,
    pub dispatch_id_for_thread: &'a Option<String>,
    pub dispatch_info_cached:
        &'a Option<crate::services::discord::router::thread_binding::DispatchInfo>,
    pub dispatch_type_str: Option<&'a str>,
    pub dispatch_uses_thread_routing: bool,
    pub is_already_thread: bool,
    pub user_text: &'a str,
    pub dispatch_effective_path: &'a str,
    pub pending_uploads:
        &'a mut crate::services::cluster::attachment_transfer::uploads::PendingUploads,
    pub session_was_cleared: Option<bool>,
}

pub(super) async fn redirect_dispatch(
    request: RedirectDispatch<'_>,
) -> Result<
    Option<(
        ChannelId,
        bool,
        Option<crate::services::discord::input_runtime::fence::Permit>,
    )>,
    Error,
> {
    let RedirectDispatch {
        http,
        cache,
        shared,
        provider,
        channel_id,
        original_channel_id,
        dispatch_id_for_thread,
        dispatch_info_cached,
        dispatch_type_str,
        dispatch_uses_thread_routing,
        is_already_thread,
        user_text,
        dispatch_effective_path,
        pending_uploads,
        session_was_cleared,
    } = request;
    let mut bootstrapped_fresh_thread_session = false;
    let mut redirected_permit = None;
    let channel_id = if let Some(did) = dispatch_id_for_thread {
        if !dispatch_uses_thread_routing {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 📢 Dispatch {did} uses primary-channel routing, skipping thread creation"
            );
            channel_id
        } else {
            // Use cached dispatch metadata for thread reuse and cross-channel role override
            let dispatch_info = dispatch_info_cached;
            let is_counter_model_dispatch =
                crate::services::dispatches::outbox_route::use_counter_model_channel(
                    dispatch_type_str,
                );
            let alt_channel_id = dispatch_info
                .as_ref()
                .and_then(|i| i.discord_channel_alt.as_deref())
                .and_then(|s| s.parse::<u64>().ok())
                .map(ChannelId::new);

            if is_already_thread {
                // Ensure thread is accessible (unarchive if needed) before proceeding
                if !super::super::super::verify_thread_accessible(http, channel_id).await {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ Dispatch {did} thread {channel_id} is not accessible (archived/locked), skipping"
                    );
                    return Ok(None);
                }
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🧵 Dispatch {did} arrived in existing thread, skipping thread creation"
                );
                // For review dispatches in reused threads, set role override
                // so this turn uses the counter-model channel's role/model.
                if is_counter_model_dispatch {
                    if let Some(alt_ch) = alt_channel_id {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 🔄 Review dispatch in reused thread: overriding role to alt channel {}",
                            alt_ch
                        );
                        let final_provider = resolve_role_binding(alt_ch, None)
                            .and_then(|binding| binding.provider)
                            .unwrap_or_else(|| provider.clone());
                        redirected_permit =
                            final_admission(&final_provider, channel_id, redirected_permit.take())?;
                        shared.dispatch.role_overrides.insert(channel_id, alt_ch);
                    }
                }
                channel_id
            } else {
                // Check if card already has an active thread via internal API
                let existing_thread = dispatch_info
                    .as_ref()
                    .and_then(|i| i.active_thread_id.clone());
                let reuse_tid = existing_thread.as_ref().and_then(|t| {
                    let id = t.parse::<u64>().unwrap_or(0);
                    if id != 0 {
                        Some(ChannelId::new(id))
                    } else {
                        None
                    }
                });

                // A reused thread is judged before it is unarchived, bootstrapped or mapped.
                if let Some(tid) = reuse_tid {
                    redirected_permit =
                        crate::services::discord::input_runtime::fence::effect::admit(
                            provider,
                            tid.get(),
                        )
                        .map_err(|failure| {
                            format!("redirect input admission refused: {failure:?}")
                        })?;
                    let input = (std::mem::take(pending_uploads), session_was_cleared);
                    let target = (tid, original_channel_id);
                    let admitted =
                        host_refusal::admitted_uploads(http, shared, provider, target, input);
                    let Some(uploads) = admitted.await? else {
                        return Ok(None);
                    };
                    *pending_uploads = uploads;
                }
                let reused = if let Some(tid) = reuse_tid {
                    if super::super::super::verify_thread_accessible(http, tid).await {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 🧵 Reusing existing thread {} for dispatch {}",
                            tid,
                            did
                        );
                        bootstrapped_fresh_thread_session = bootstrap_admitted(
                            redirected_permit.clone(),
                            shared,
                            tid,
                            dispatch_effective_path,
                            http,
                            cache,
                        )
                        .await;
                        shared.dispatch.thread_parents.insert(channel_id, tid);
                        // For review dispatches reusing an implementation thread,
                        // override role/model to use the counter-model channel.
                        if is_counter_model_dispatch {
                            if let Some(alt_ch) = alt_channel_id {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!(
                                    "  [{ts}] 🔄 Review dispatch reusing thread: overriding role to alt channel {}",
                                    alt_ch
                                );
                                let final_provider = resolve_role_binding(alt_ch, None)
                                    .and_then(|binding| binding.provider)
                                    .unwrap_or_else(|| provider.clone());
                                redirected_permit = final_admission(
                                    &final_provider,
                                    tid,
                                    redirected_permit.take(),
                                )?;
                                shared.dispatch.role_overrides.insert(tid, alt_ch);
                            }
                        }
                        Some(tid)
                    } else {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 🧵 Thread {} is locked/inaccessible, creating new for {}",
                            tid,
                            did
                        );
                        None
                    }
                } else {
                    None
                };

                if let Some(tid) = reused {
                    tid
                } else {
                    // No existing usable thread — create new
                    let thread_title = user_text
                        .find(" - ")
                        .map(|idx| &user_text[idx + 3..])
                        .unwrap_or("dispatch")
                        .chars()
                        .take(90)
                        .collect::<String>();

                    match channel_id
                        .create_thread(
                            http,
                            poise::serenity_prelude::builder::CreateThread::new(thread_title)
                                .kind(poise::serenity_prelude::ChannelType::PublicThread)
                                .auto_archive_duration(
                                    poise::serenity_prelude::AutoArchiveDuration::OneDay,
                                ),
                        )
                        .await
                    {
                        Ok(thread) => {
                            redirected_permit =
                                crate::services::discord::input_runtime::fence::effect::admit(
                                    provider,
                                    thread.id.get(),
                                )
                                .map_err(|failure| {
                                    format!("redirect input admission refused: {failure:?}")
                                })?;
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!(
                                "  [{ts}] 🧵 Created dispatch thread {} for dispatch {}",
                                thread.id,
                                did
                            );
                            bootstrapped_fresh_thread_session = bootstrap_admitted(
                                redirected_permit.clone(),
                                shared,
                                thread.id,
                                dispatch_effective_path,
                                http,
                                cache,
                            )
                            .await;
                            shared.dispatch.thread_parents.insert(channel_id, thread.id);
                            super::super::super::link_dispatch_thread(
                                shared.api_port,
                                did,
                                thread.id.get(),
                                channel_id.get(),
                            )
                            .await;
                            thread.id
                        }
                        Err(e) => {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!("  [{ts}] ⚠ Failed to create dispatch thread: {e}");
                            channel_id // fallback to main channel
                        }
                    }
                }
            }
        }
    } else {
        channel_id
    };
    Ok(Some((
        channel_id,
        bootstrapped_fresh_thread_session,
        redirected_permit,
    )))
}

pub(super) fn final_admission(
    provider: &ProviderKind,
    channel: ChannelId,
    redirected: Option<crate::services::discord::input_runtime::fence::Permit>,
) -> Result<Option<crate::services::discord::input_runtime::fence::Permit>, Error> {
    match redirected {
        Some(permit) if permit.names(provider, channel.get()) => {
            permit
                .validate(provider, channel.get())
                .map_err(|failure| format!("final input admission refused: {failure:?}"))?;
            Ok(Some(permit))
        }
        _ => crate::services::discord::input_runtime::fence::effect::admit(provider, channel.get())
            .map_err(|failure| format!("final input admission refused: {failure:?}").into()),
    }
}

async fn bootstrap_admitted(
    permit: Option<crate::services::discord::input_runtime::fence::Permit>,
    shared: &Arc<SharedData>,
    channel: ChannelId,
    path: &str,
    http: &Arc<serenity::http::Http>,
    cache: Option<&Arc<serenity::cache::Cache>>,
) -> bool {
    use crate::services::discord::input_runtime::fence::effect;
    if permit.is_none() && effect::current().is_none() {
        return super::super::super::super::bootstrap_thread_session(
            shared, channel, path, http, cache,
        )
        .await;
    }
    let (shared, path, http, cache) = (
        shared.clone(),
        path.to_owned(),
        http.clone(),
        cache.cloned(),
    );
    effect::run(permit, async move {
        super::super::super::super::bootstrap_thread_session(
            &shared,
            channel,
            &path,
            &http,
            cache.as_ref(),
        )
        .await
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::input_runtime::fence::{self, Gate, Mode};
    use futures::FutureExt;

    #[cfg(unix)]
    #[tokio::test]
    async fn c1b_final_refusal_preserves_existing_thread_role_override() {
        if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
            "services::discord::router::message_handler::intake_turn::adk_thread::tests::",
            "c1b_final_refusal_preserves_existing_thread_role_override"
        )) {
            return;
        }
        let _root = crate::config::TestRuntimeRootGuard::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(6_325_539);
        let alt = ChannelId::new(6_325_540);
        let previous = ChannelId::new(6_325_541);
        let path = crate::services::discord::runtime_store::role_map_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({"byChannelId": {alt.to_string(): {
                "roleId": "c1b-refused-role", "promptFile": "", "provider": "codex"
            }}})
            .to_string(),
        )
        .unwrap();
        let gate = Gate::protect(ProviderKind::Codex, channel.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let closing = gate.close().unwrap();
        shared.dispatch.role_overrides.insert(channel, previous);
        let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(
            Arc::new(move |_, path| {
                path.ends_with(&format!("/channels/{channel}")).then(|| {
                    serde_json::json!({
                        "id": channel.to_string(), "type": 0, "name": "c1b-input", "position": 0
                    })
                })
            }),
        )
        .await;
        let info = crate::services::discord::router::thread_binding::DispatchInfo {
            discord_channel_alt: Some(alt.to_string()),
            ..Default::default()
        };
        let mut uploads = Vec::new();
        let result = redirect_dispatch(RedirectDispatch {
            http: &api.http,
            cache: None,
            shared: &shared,
            provider: &shared.provider,
            channel_id: channel,
            original_channel_id: channel,
            dispatch_id_for_thread: &Some("c1b-refused-dispatch".into()),
            dispatch_info_cached: &Some(info),
            dispatch_type_str: Some("review"),
            dispatch_uses_thread_routing: true,
            is_already_thread: true,
            user_text: "review",
            dispatch_effective_path: "/unreachable",
            pending_uploads: &mut uploads,
            session_was_cleared: Some(false),
        })
        .await;
        assert!(result.err().unwrap().to_string().contains("Closing"));
        assert_eq!(
            *shared.dispatch.role_overrides.get(&channel).unwrap(),
            previous,
            "refused final identity must not replace the prior override"
        );
        closing.drain().await;
    }

    #[tokio::test]
    async fn c1b_final_identity_separates_original_and_execution_provider() {
        let source = Gate::protect(ProviderKind::Codex, 6_325_510).unwrap();
        let destination = Gate::protect(ProviderKind::Claude, 6_325_511).unwrap();
        let _source_health = fence::test_health::Clear::new(&source);
        let _destination_health = fence::test_health::Clear::new(&destination);
        let permit = source.admit().unwrap();
        let source_closing = source.close().unwrap();
        let destination_closing = destination.close().unwrap();
        fence::effect::detached(Some(permit), async move {
            let inherited = fence::effect::current().unwrap();
            let same = final_admission(
                &ProviderKind::Codex,
                ChannelId::new(6_325_510),
                Some(inherited.clone()),
            )
            .unwrap();
            assert!(same.unwrap().names(&ProviderKind::Codex, 6_325_510));
            let refused = final_admission(
                &ProviderKind::Claude,
                ChannelId::new(6_325_511),
                Some(inherited.clone()),
            );
            assert!(refused.err().unwrap().to_string().contains("Closing"));
            assert!(
                final_admission(
                    &ProviderKind::Claude,
                    ChannelId::new(6_325_510),
                    Some(inherited)
                )
                .unwrap()
                .is_none()
            );
        })
        .await;
        source_closing.drain().await;
        destination_closing.drain().await;
    }

    #[tokio::test]
    async fn c1b_redirect_refuses_closed_thread_before_bootstrap_or_mapping() {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let original = ChannelId::new(6_325_508);
        let destination = ChannelId::new(6_325_509);
        let gate = Gate::protect(shared.provider.clone(), destination.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let closing = gate.close().unwrap();
        let http = Arc::new(serenity::http::Http::new("Bot input-test-no-network"));
        let info = crate::services::discord::router::thread_binding::DispatchInfo {
            active_thread_id: Some(destination.to_string()),
            ..Default::default()
        };
        let mut uploads = Vec::new();
        let result = redirect_dispatch(RedirectDispatch {
            http: &http,
            cache: None,
            shared: &shared,
            provider: &shared.provider,
            channel_id: original,
            original_channel_id: original,
            dispatch_id_for_thread: &Some("dispatch-test".into()),
            dispatch_info_cached: &Some(info),
            dispatch_type_str: None,
            dispatch_uses_thread_routing: true,
            is_already_thread: false,
            user_text: "input",
            dispatch_effective_path: "/unreachable",
            pending_uploads: &mut uploads,
            session_was_cleared: Some(false),
        })
        .now_or_never()
        .expect("closed thread cannot await network");
        assert!(result.err().unwrap().to_string().contains("Closing"));
        assert!(!shared.dispatch.thread_parents.contains_key(&original));
        assert!(!shared.core.lock().await.sessions.contains_key(&destination));
        assert_eq!(gate.mode(), Mode::Closing);
        closing.drain().await;
    }
}
