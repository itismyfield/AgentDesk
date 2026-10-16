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
    pub is_already_thread: bool,
    pub user_text: &'a str,
    pub dispatch_effective_path: &'a str,
    pub pending_uploads:
        &'a mut crate::services::cluster::attachment_transfer::uploads::PendingUploads,
    pub session_was_cleared: Option<bool>,
    pub queued_drain: bool,
    pub queued: Option<Route>,
}

/// Where a turn runs and the routing it owns. Nothing is written until its mailbox claim
/// succeeds, so a turn that has not claimed cannot be taken for the channel's owner.
pub(super) type Route = crate::services::turn_orchestrator::QueuedRoute;

impl Route {
    /// The thread edge that points at `target` as read at resolve time.
    fn parent_of(shared: &SharedData, target: ChannelId) -> Option<ChannelId> {
        (shared.dispatch.thread_parents.iter())
            .filter(|edge| *edge.value() == target)
            .map(|edge| *edge.key())
            .min()
    }

    /// No routing decision: the channel's override as read at resolve time.
    fn inherited_role(shared: &SharedData, target: ChannelId) -> Option<ChannelId> {
        shared.dispatch.role_overrides.get(&target).map(|v| *v)
    }

    pub(super) fn provider(&self, provider: &ProviderKind) -> ProviderKind {
        (self.role)
            .and_then(|role| resolve_role_binding(role, None))
            .and_then(|binding| binding.provider)
            .unwrap_or_else(|| provider.clone())
    }

    /// Runs right after the claim: this turn's parent edge and override replace the channel's.
    /// A replaced parent loses its thread guard, so its queue is kicked like any edge release.
    pub(super) fn publish(&self, shared: &Arc<SharedData>, provider: &ProviderKind) {
        let mut replaced = Vec::new();
        shared.dispatch.thread_parents.retain(|parent, thread| {
            let remove = *thread == self.target && Some(*parent) != self.parent;
            if remove {
                replaced.push(*parent);
            }
            !remove
        });
        if let Some(parent) = self.parent {
            shared.dispatch.thread_parents.insert(parent, self.target);
        }
        use crate::services::discord::turn_finalizer::cleanup;
        cleanup::kickoff_thread_parents_after_finalize(shared, provider, replaced);
        match self.role {
            Some(role) => _ = shared.dispatch.role_overrides.insert(self.target, role),
            None => _ = shared.dispatch.role_overrides.remove(&self.target),
        }
    }
}

pub(super) async fn redirect_dispatch(
    request: RedirectDispatch<'_>,
) -> Result<
    Option<(
        Route,
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
        is_already_thread,
        user_text,
        dispatch_effective_path,
        pending_uploads,
        session_was_cleared,
        queued_drain,
        queued,
    } = request;
    let mut bootstrapped_fresh_thread_session = false;
    let mut redirected_permit = None;
    let (mut parent, mut role) = (None, None);
    let channel_id = if let Some(did) = dispatch_id_for_thread {
        if !crate::dispatch::dispatch_type_uses_thread_routing(dispatch_type_str) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 📢 Dispatch {did} uses primary-channel routing, skipping thread creation"
            );
            role = Route::inherited_role(shared, channel_id);
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
                        role = Some(alt_ch);
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
                    let target = (tid, original_channel_id, queued_drain);
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
                        parent = Some(channel_id);
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
                                role = Some(alt_ch);
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
                            parent = Some(channel_id);
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
                            role = Route::inherited_role(shared, channel_id);
                            channel_id // fallback to main channel
                        }
                    }
                }
            }
        }
    } else {
        role = Route::inherited_role(shared, channel_id);
        channel_id
    };
    // A replayed item keeps the route it resolved before it lost its claim.
    let route = match queued {
        Some(queued) if queued.target == channel_id => queued,
        _ => Route {
            target: channel_id,
            parent: parent.or_else(|| Route::parent_of(shared, channel_id)),
            role,
        },
    };
    #[cfg(test)]
    route_reached(route.target).await;
    Ok(Some((
        route,
        bootstrapped_fresh_thread_session,
        redirected_permit,
    )))
}

#[cfg(all(test, unix))]
#[path = "route_claim_tests.rs"]
mod route_claim_tests;

#[cfg(test)]
type RouteHold = (
    ChannelId,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
#[cfg(test)]
static ROUTE_HOLD: std::sync::Mutex<Option<RouteHold>> = std::sync::Mutex::new(None);

/// Test seam: holds the next route resolved to `target` before its claim; the first receiver fires
/// there and the route resumes once the returned sender fires.
#[cfg(test)]
fn hold_route(
    target: ChannelId,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (reached, at) = tokio::sync::oneshot::channel();
    let (release, resume) = tokio::sync::oneshot::channel();
    *ROUTE_HOLD.lock().unwrap() = Some((target, reached, resume));
    (at, release)
}

#[cfg(test)]
async fn route_reached(target: ChannelId) {
    let held = {
        let mut hold = ROUTE_HOLD.lock().unwrap();
        match hold.take() {
            Some((at, reached, resume)) if at == target => Some((reached, resume)),
            other => {
                *hold = other;
                None
            }
        }
    };
    if let Some((reached, resume)) = held {
        let _ = reached.send(());
        let _ = resume.await;
    }
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
            is_already_thread: true,
            user_text: "review",
            dispatch_effective_path: "/unreachable",
            pending_uploads: &mut uploads,
            session_was_cleared: Some(false),
            queued_drain: false,
            queued: None,
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
            // Another provider's protection covers the channel, so a foreign writer is refused.
            let foreign = final_admission(
                &ProviderKind::Claude,
                ChannelId::new(6_325_510),
                Some(inherited),
            );
            assert!(foreign.err().unwrap().to_string().contains("Closing"));
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
            is_already_thread: false,
            user_text: "input",
            dispatch_effective_path: "/unreachable",
            pending_uploads: &mut uploads,
            session_was_cleared: Some(false),
            queued_drain: false,
            queued: None,
        })
        .now_or_never()
        .expect("closed thread cannot await network");
        assert!(result.err().unwrap().to_string().contains("Closing"));
        assert!(!shared.dispatch.thread_parents.contains_key(&original));
        assert!(!shared.core.lock().await.sessions.contains_key(&destination));
        assert_eq!(gate.mode(), Mode::Closing);
        closing.drain().await;
    }

    /// A queued input refused by its reused Herdr thread hands every taken upload to its queue
    /// owner once; the session gets back only its clear flag and keeps an upload that came later.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_refused_queued_redirect_returns_its_uploads_and_clear_flag() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        // Fence gates stay registered for the whole process, so no other test may use these.
        let original = ChannelId::new(7_024_101);
        let reused = ChannelId::new(7_024_102);
        // Without a database the configured thread's row is unreadable, so Herdr refuses it.
        let hosts = [(reused.get(), "mac-mini")];
        let _hosts = crate::config::session_hosts::force_for_test(Some("mac-mini"), &hosts);
        let map = crate::services::discord::host_defer_gate::tests::map_channel;
        map(&shared, original, "redirect-input").await;
        let upload = |record: &str| {
            crate::services::cluster::attachment_transfer::uploads::Upload::Local(record.into())
        };
        let (taken, queued, late) = (upload("taken"), upload("queued"), upload("late"));
        // The intake already took the session's upload and clear flag; another upload arrived since.
        let late_only = vec![late];
        let mut core = shared.core.lock().await;
        core.sessions.get_mut(&original).unwrap().pending_uploads = late_only.clone();
        drop(core);
        let info = crate::services::discord::router::thread_binding::DispatchInfo {
            active_thread_id: Some(reused.to_string()),
            ..Default::default()
        };
        let http = Arc::new(serenity::http::Http::new("Bot input-test-no-network"));
        let mut uploads = vec![taken.clone(), queued.clone()];
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
            is_already_thread: false,
            user_text: "input",
            dispatch_effective_path: "/unreachable",
            pending_uploads: &mut uploads,
            session_was_cleared: Some(true),
            queued_drain: true,
            queued: None,
        })
        .now_or_never()
        .expect("a queued refusal sends no notice");
        type Handoff =
            crate::services::discord::router::intake_dispatch::QueuedTransitionBusyHandoff;
        let handoff = result
            .err()
            .expect("the refusal goes back to the queue owner")
            .downcast::<Handoff>()
            .expect("a busy handoff");
        assert_eq!(
            handoff.owned_uploads,
            vec![taken, queued],
            "each taken upload goes back to the queue once"
        );
        let core = shared.core.lock().await;
        let session = core.sessions.get(&original).unwrap();
        assert!(
            session.cleared,
            "the redirect's clear flag returns to the session"
        );
        assert_eq!(
            session.pending_uploads, late_only,
            "only the later upload stays on the session"
        );
        assert!(!core.sessions.contains_key(&reused), "no thread bootstrap");
        drop(core);
        assert!(!shared.dispatch.thread_parents.contains_key(&original));
    }
}
