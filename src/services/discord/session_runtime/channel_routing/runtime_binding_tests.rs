use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{Map, Value, json};
use tempfile::TempDir;

use super::*;

const CHILD_ID: u64 = 1504612455916245999;
const PARENT_ID: u64 = 1479671301387059200;
const WRONG_PARENT_ID: u64 = 1479671301387059299;
const CHILD_NAME: &str = "direct-review-thread";
const PARENT_NAME: &str = "parent-agent-channel";

#[derive(Clone, Copy, Debug)]
enum DirectBindingKind {
    Id,
    Name,
}

#[derive(Clone, Copy, Debug)]
enum PayloadKind {
    Role,
    Workspace,
    Both,
}

#[derive(Clone, Copy, Debug)]
enum ParentShape {
    Unbound,
    OptedOut,
    WrongNameTarget,
}

fn role_binding(role_id: &str, provider: &str) -> Value {
    json!({
        "roleId": role_id,
        "promptFile": format!("/tmp/{role_id}.md"),
        "provider": provider,
    })
}

fn configured_payload(kind: PayloadKind, role_id: &str, workspace: &str) -> Value {
    match kind {
        PayloadKind::Role => role_binding(role_id, "claude"),
        PayloadKind::Workspace => json!({"workspace": workspace}),
        PayloadKind::Both => {
            let mut payload = role_binding(role_id, "claude");
            payload["workspace"] = json!(workspace);
            payload
        }
    }
}

fn direct_child_role_map(
    direct_kind: DirectBindingKind,
    payload_kind: PayloadKind,
    parent_shape: ParentShape,
) -> Value {
    let mut by_id = Map::new();
    let mut by_name = Map::new();
    let child_payload =
        configured_payload(payload_kind, "review-agent", "/tmp/direct-child-workspace");

    match direct_kind {
        DirectBindingKind::Id => {
            by_id.insert(CHILD_ID.to_string(), child_payload);
        }
        DirectBindingKind::Name => {
            let mut child = child_payload;
            child["channelId"] = json!(CHILD_ID.to_string());
            by_name.insert(CHILD_NAME.to_string(), child);
        }
    }

    match parent_shape {
        ParentShape::Unbound => {}
        ParentShape::OptedOut => {
            let mut parent = role_binding("project-agentdesk", "codex");
            parent["threadInherit"] = json!(false);
            by_id.insert(PARENT_ID.to_string(), parent);
        }
        ParentShape::WrongNameTarget => {
            let mut parent = role_binding("wrong-parent-agent", "codex");
            parent["channelId"] = json!(WRONG_PARENT_ID.to_string());
            by_name.insert(PARENT_NAME.to_string(), parent);
        }
    }

    json!({
        "fallbackByChannelName": {"enabled": true},
        "byChannelId": by_id,
        "byChannelName": by_name,
    })
}

fn with_role_map<F>(role_map: Value, test: F)
where
    F: FnOnce(),
{
    let temp = TempDir::new().expect("temporary AgentDesk root");
    let root = temp.path().join(".adk");
    let config = root.join("config");
    fs::create_dir_all(&config).expect("create config directory");
    fs::write(config.join("role_map.json"), role_map.to_string()).expect("write role map");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", &root);
    test();
}

async fn with_role_map_async<F, Fut>(role_map: Value, test: F)
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: Future<Output = ()>,
{
    let temp = TempDir::new().expect("temporary AgentDesk root");
    let root = temp.path().join(".adk");
    let config = root.join("config");
    fs::create_dir_all(&config).expect("create config directory");
    let role_map_path = config.join("role_map.json");
    fs::write(&role_map_path, role_map.to_string()).expect("write role map");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", &root);
    test(role_map_path).await;
}

type LookupMutation = Box<dyn FnOnce() + Send>;

struct FakeRuntimeChannelMetadataLookup {
    responses: HashMap<serenity::ChannelId, Result<RuntimeChannelMetadata, ()>>,
    calls: Mutex<Vec<serenity::ChannelId>>,
    mutations: Mutex<HashMap<serenity::ChannelId, LookupMutation>>,
}

impl FakeRuntimeChannelMetadataLookup {
    fn new(
        responses: impl IntoIterator<Item = (serenity::ChannelId, Result<RuntimeChannelMetadata, ()>)>,
    ) -> Self {
        Self {
            responses: responses.into_iter().collect(),
            calls: Mutex::new(Vec::new()),
            mutations: Mutex::new(HashMap::new()),
        }
    }

    fn mutate_during_lookup(
        mut self,
        channel_id: serenity::ChannelId,
        mutation: impl FnOnce() + Send + 'static,
    ) -> Self {
        self.mutations
            .get_mut()
            .expect("mutation lock")
            .insert(channel_id, Box::new(mutation));
        self
    }

    fn call_count(&self, channel_id: serenity::ChannelId) -> usize {
        self.calls
            .lock()
            .expect("call lock")
            .iter()
            .filter(|called| **called == channel_id)
            .count()
    }
}

#[async_trait::async_trait]
impl RuntimeChannelMetadataLookup for FakeRuntimeChannelMetadataLookup {
    async fn lookup_channel(
        &self,
        channel_id: serenity::ChannelId,
    ) -> Result<RuntimeChannelMetadata, ()> {
        self.calls.lock().expect("call lock").push(channel_id);
        if let Some(mutation) = self
            .mutations
            .lock()
            .expect("mutation lock")
            .remove(&channel_id)
        {
            mutation();
        }
        self.responses.get(&channel_id).cloned().unwrap_or(Err(()))
    }
}

fn guild_metadata(name: &str, is_thread: bool, parent_id: Option<u64>) -> RuntimeChannelMetadata {
    RuntimeChannelMetadata::Guild {
        name: name.to_string(),
        is_thread,
        parent_id: parent_id.map(serenity::ChannelId::new),
    }
}

fn classify_thread(child_name: &str, parent_name: &str) -> RuntimeChannelBindingResolution {
    classify_runtime_channel_binding_from_live_metadata(
        serenity::ChannelId::new(CHILD_ID),
        Some(child_name),
        Some((serenity::ChannelId::new(PARENT_ID), Some(parent_name))),
    )
}

fn authority_payload(
    resolution: &RuntimeChannelBindingResolution,
) -> (&settings::ConfiguredBindingPayload, bool) {
    match resolution.authority().expect("configured authority") {
        RuntimeBindingAuthority::Direct { payload, .. } => (payload, true),
        RuntimeBindingAuthority::InheritedParent { payload, .. } => (payload, false),
    }
}

fn missing_mention_is_skipped(
    resolution: &RuntimeChannelBindingResolution,
    required_channel_ids: &[u64],
) -> bool {
    let settings = DiscordBotSettings {
        require_mention_channel_ids: required_channel_ids.to_vec(),
        ..Default::default()
    };
    crate::services::discord::router::should_skip_for_missing_required_mention(
        &settings,
        resolution.authority_channel_id(),
        false,
        "plain message without a mention",
        serenity::UserId::new(1234),
    )
}

#[test]
fn direct_child_id_and_name_payload_matrix_outranks_every_parent_shape() {
    for direct_kind in [DirectBindingKind::Id, DirectBindingKind::Name] {
        for payload_kind in [PayloadKind::Role, PayloadKind::Workspace, PayloadKind::Both] {
            for parent_shape in [
                ParentShape::Unbound,
                ParentShape::OptedOut,
                ParentShape::WrongNameTarget,
            ] {
                with_role_map(
                    direct_child_role_map(direct_kind, payload_kind, parent_shape),
                    || {
                        let resolution = classify_thread(CHILD_NAME, PARENT_NAME);
                        let (payload, direct) = authority_payload(&resolution);
                        assert!(direct);
                        assert_eq!(
                            resolution.status(),
                            RuntimeChannelBindingStatus::Owned,
                            "direct {direct_kind:?}/{payload_kind:?} child must outrank {parent_shape:?} parent"
                        );
                        assert_eq!(
                            resolution.authority_channel_id(),
                            serenity::ChannelId::new(CHILD_ID),
                            "direct {direct_kind:?}/{payload_kind:?} owns mention policy under {parent_shape:?}"
                        );
                        assert_eq!(
                            payload.role.is_some(),
                            matches!(payload_kind, PayloadKind::Role | PayloadKind::Both)
                        );
                        assert_eq!(
                            payload.workspace.is_some(),
                            matches!(payload_kind, PayloadKind::Workspace | PayloadKind::Both)
                        );
                        assert!(missing_mention_is_skipped(&resolution, &[CHILD_ID]));
                        assert!(!missing_mention_is_skipped(&resolution, &[PARENT_ID]));
                    },
                );
            }
        }
    }
}

#[test]
fn direct_role_workspace_and_combined_payloads_are_one_child_authority() {
    for (label, mut child, expect_role, expect_workspace) in [
        (
            "role-only",
            role_binding("review-agent", "claude"),
            true,
            false,
        ),
        (
            "workspace-only",
            json!({"workspace": "/tmp/direct-child"}),
            false,
            true,
        ),
        (
            "combined",
            {
                let mut entry = role_binding("review-agent", "claude");
                entry["workspace"] = json!("/tmp/direct-child");
                entry
            },
            true,
            true,
        ),
    ] {
        child["channelId"] = json!(CHILD_ID.to_string());
        with_role_map(
            json!({
                "fallbackByChannelName": {"enabled": true},
                "byChannelId": {
                    PARENT_ID.to_string(): {
                        "roleId": "parent-agent",
                        "promptFile": "/tmp/parent.md",
                        "provider": "codex",
                        "workspace": "/tmp/parent",
                    },
                },
                "byChannelName": {CHILD_NAME: child},
            }),
            || {
                let resolution = classify_thread(CHILD_NAME, PARENT_NAME);
                let (payload, direct) = authority_payload(&resolution);
                assert!(direct, "{label} must establish a direct barrier");
                assert_eq!(payload.role.is_some(), expect_role, "{label}");
                assert_eq!(payload.workspace.is_some(), expect_workspace, "{label}");
                assert_eq!(
                    resolution
                        .authority_identity()
                        .and_then(RuntimeChannelIdentity::channel_name),
                    Some(CHILD_NAME),
                    "{label} memory/binding identity must retain the actual live child name"
                );
                assert_ne!(
                    payload.workspace.as_deref(),
                    Some("/tmp/parent"),
                    "a direct role-only child must not steal the parent workspace"
                );
                assert_ne!(
                    payload.role.as_ref().map(|role| role.role_id.as_str()),
                    Some("parent-agent"),
                    "a direct workspace-only child must not steal the parent role"
                );
            },
        );
    }
}

#[test]
fn inherited_parent_payload_is_selected_only_as_one_complete_parent_authority() {
    for thread_inherit in [false, true] {
        with_role_map(
            json!({
                "byChannelId": {
                    PARENT_ID.to_string(): {
                        "roleId": "parent-agent",
                        "promptFile": "/tmp/parent.md",
                        "provider": "codex",
                        "workspace": "/tmp/parent",
                        "threadInherit": thread_inherit,
                    },
                },
            }),
            || {
                let resolution = classify_thread("unbound-child", PARENT_NAME);
                if thread_inherit {
                    let (payload, direct) = authority_payload(&resolution);
                    assert!(!direct);
                    assert_eq!(payload.workspace.as_deref(), Some("/tmp/parent"));
                    assert_eq!(
                        payload.role.as_ref().map(|role| role.role_id.as_str()),
                        Some("parent-agent")
                    );
                    assert_eq!(
                        resolution
                            .authority_identity()
                            .expect("parent authority")
                            .channel_id(),
                        serenity::ChannelId::new(PARENT_ID)
                    );
                } else {
                    assert_eq!(resolution.status(), RuntimeChannelBindingStatus::Unowned);
                    assert!(resolution.authority().is_none());
                }
            },
        );
    }
}

#[test]
fn strict_id_payload_fills_only_from_an_exact_id_pinned_live_name() {
    let cases = [
        (
            json!({"workspace": "/tmp/strict"}),
            role_binding("pinned-role", "claude"),
            true,
            Some("/tmp/strict"),
        ),
        (
            role_binding("strict-role", "codex"),
            json!({"workspace": "/tmp/pinned"}),
            true,
            Some("/tmp/pinned"),
        ),
    ];
    for (strict, mut by_name, expect_role, expect_workspace) in cases {
        by_name["channelId"] = json!(CHILD_ID.to_string());
        with_role_map(
            json!({
                "fallbackByChannelName": {"enabled": true},
                "byChannelId": {CHILD_ID.to_string(): strict},
                "byChannelName": {CHILD_NAME: by_name},
            }),
            || {
                let resolution = classify_thread(CHILD_NAME, PARENT_NAME);
                let (payload, direct) = authority_payload(&resolution);
                assert!(direct);
                assert_eq!(payload.role.is_some(), expect_role);
                assert_eq!(payload.workspace.as_deref(), expect_workspace);
            },
        );
    }

    with_role_map(
        json!({
            "fallbackByChannelName": {"enabled": true},
            "byChannelId": {CHILD_ID.to_string(): {"workspace": "/tmp/strict"}},
            "byChannelName": {CHILD_NAME: role_binding("unpinned-role", "claude")},
        }),
        || {
            let resolution = classify_thread(CHILD_NAME, PARENT_NAME);
            let (payload, _) = authority_payload(&resolution);
            assert!(
                payload.role.is_none(),
                "unpinned names cannot fill strict payloads"
            );
            assert_eq!(payload.workspace.as_deref(), Some("/tmp/strict"));
        },
    );

    with_role_map(
        json!({
            "fallbackByChannelName": {"enabled": true},
            "byChannelId": {CHILD_ID.to_string(): role_binding("strict-role", "codex")},
            "byChannelName": {CHILD_NAME: {"workspace": "/tmp/unpinned"}},
        }),
        || {
            let resolution = classify_thread(CHILD_NAME, PARENT_NAME);
            let (payload, _) = authority_payload(&resolution);
            assert_eq!(
                payload.role.as_ref().map(|role| role.role_id.as_str()),
                Some("strict-role")
            );
            assert!(
                payload.workspace.is_none(),
                "unpinned names cannot fill the symmetric strict-role payload"
            );
        },
    );
}

#[test]
fn strict_id_fields_and_thread_inherit_win_exact_pinned_conflicts() {
    let mut pinned = role_binding("pinned-role", "claude");
    pinned["channelId"] = json!(CHILD_ID.to_string());
    pinned["workspace"] = json!("/tmp/pinned");
    pinned["threadInherit"] = json!(true);
    with_role_map(
        json!({
            "fallbackByChannelName": {"enabled": true},
            "byChannelId": {
                CHILD_ID.to_string(): {
                    "roleId": "strict-role",
                    "promptFile": "/tmp/strict.md",
                    "provider": "codex",
                    "workspace": "/tmp/strict",
                    "threadInherit": false,
                },
            },
            "byChannelName": {CHILD_NAME: pinned},
        }),
        || {
            let resolution = classify_thread(CHILD_NAME, PARENT_NAME);
            let (payload, direct) = authority_payload(&resolution);
            assert!(direct);
            assert_eq!(
                payload.role.as_ref().map(|role| role.role_id.as_str()),
                Some("strict-role")
            );
            assert_eq!(payload.workspace.as_deref(), Some("/tmp/strict"));
            assert!(!payload.thread_inherit);
        },
    );
}

#[test]
fn captured_strict_snapshot_is_not_reresolved_after_metadata_boundary() {
    let temp = TempDir::new().expect("temporary AgentDesk root");
    let root = temp.path().join(".adk");
    let config = root.join("config");
    fs::create_dir_all(&config).expect("create config directory");
    let role_map_path = config.join("role_map.json");
    fs::write(
        &role_map_path,
        json!({
            "fallbackByChannelName": {"enabled": true},
            "byChannelId": {CHILD_ID.to_string(): role_binding("captured-strict", "codex")},
        })
        .to_string(),
    )
    .expect("write initial role map");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", &root);
    let strict =
        settings::resolve_runtime_strict_configured_binding(serenity::ChannelId::new(CHILD_ID))
            .expect("capture strict snapshot before metadata await");

    let mut pinned = json!({"workspace": "/tmp/pinned-fill"});
    pinned["channelId"] = json!(CHILD_ID.to_string());
    fs::write(
        &role_map_path,
        json!({
            "fallbackByChannelName": {"enabled": true},
            "byChannelId": {
                CHILD_ID.to_string(): {
                    "roleId": "late-reloaded-role",
                    "promptFile": "/tmp/late.md",
                    "workspace": "/tmp/late-reloaded-workspace",
                },
            },
            "byChannelName": {CHILD_NAME: pinned},
        })
        .to_string(),
    )
    .expect("replace config across simulated metadata await");

    let merged = settings::merge_runtime_configured_binding_with_pinned_name(
        serenity::ChannelId::new(CHILD_ID),
        Some(CHILD_NAME),
        Some(strict),
    )
    .expect("merge captured strict with exact pinned fill");
    assert_eq!(
        merged.role.as_ref().map(|role| role.role_id.as_str()),
        Some("captured-strict"),
        "the strict payload must not be reloaded after the metadata boundary"
    );
    assert_eq!(merged.workspace.as_deref(), Some("/tmp/pinned-fill"));
}

#[test]
fn parent_thread_inherit_uses_strict_precedence_and_pinned_false_when_unopposed() {
    let mut pinned_parent = role_binding("pinned-parent", "claude");
    pinned_parent["channelId"] = json!(PARENT_ID.to_string());
    pinned_parent["workspace"] = json!("/tmp/pinned-parent");
    pinned_parent["threadInherit"] = json!(false);
    with_role_map(
        json!({
            "fallbackByChannelName": {"enabled": true},
            "byChannelName": {PARENT_NAME: pinned_parent.clone()},
        }),
        || {
            let resolution = classify_thread("unbound-child", PARENT_NAME);
            assert_eq!(resolution.status(), RuntimeChannelBindingStatus::Unowned);
            assert!(resolution.authority().is_none());
        },
    );

    with_role_map(
        json!({
            "fallbackByChannelName": {"enabled": true},
            "byChannelId": {
                PARENT_ID.to_string(): {
                    "roleId": "strict-parent",
                    "promptFile": "/tmp/strict-parent.md",
                    "provider": "codex",
                    "threadInherit": true,
                },
            },
            "byChannelName": {PARENT_NAME: pinned_parent},
        }),
        || {
            let resolution = classify_thread("unbound-child", PARENT_NAME);
            let (payload, direct) = authority_payload(&resolution);
            assert!(!direct);
            assert!(payload.thread_inherit, "strict ID threadInherit must win");
            assert_eq!(
                payload.role.as_ref().map(|role| role.role_id.as_str()),
                Some("strict-parent")
            );
        },
    );
}

#[test]
fn thread_inherit_tri_state_defaults_only_after_strict_and_pinned_merge() {
    struct Case {
        label: &'static str,
        strict_flag: Option<bool>,
        pinned_flag: Option<bool>,
        include_strict: bool,
        include_pinned: bool,
        expected: bool,
    }

    for case in [
        Case {
            label: "strict absent plus pinned false",
            strict_flag: None,
            pinned_flag: Some(false),
            include_strict: true,
            include_pinned: true,
            expected: false,
        },
        Case {
            label: "strict true outranks pinned false",
            strict_flag: Some(true),
            pinned_flag: Some(false),
            include_strict: true,
            include_pinned: true,
            expected: true,
        },
        Case {
            label: "strict false outranks pinned true",
            strict_flag: Some(false),
            pinned_flag: Some(true),
            include_strict: true,
            include_pinned: true,
            expected: false,
        },
        Case {
            label: "pinned-only false",
            strict_flag: None,
            pinned_flag: Some(false),
            include_strict: false,
            include_pinned: true,
            expected: false,
        },
        Case {
            label: "neither source specifies a flag",
            strict_flag: None,
            pinned_flag: None,
            include_strict: true,
            include_pinned: true,
            expected: true,
        },
    ] {
        let mut by_id = Map::new();
        if case.include_strict {
            let mut strict = role_binding("strict-parent", "codex");
            if let Some(flag) = case.strict_flag {
                strict["threadInherit"] = json!(flag);
            }
            by_id.insert(PARENT_ID.to_string(), strict);
        }
        let mut by_name = Map::new();
        if case.include_pinned {
            let mut pinned = role_binding("pinned-parent", "claude");
            pinned["channelId"] = json!(PARENT_ID.to_string());
            if let Some(flag) = case.pinned_flag {
                pinned["threadInherit"] = json!(flag);
            }
            by_name.insert(PARENT_NAME.to_string(), pinned);
        }

        with_role_map(
            json!({
                "fallbackByChannelName": {"enabled": true},
                "byChannelId": by_id,
                "byChannelName": by_name,
            }),
            || {
                let payload = settings::resolve_runtime_configured_binding(
                    serenity::ChannelId::new(PARENT_ID),
                    Some(PARENT_NAME),
                )
                .expect("parent owns the configured scope");
                assert_eq!(payload.thread_inherit, case.expected, "{}", case.label);

                let resolution = classify_thread("unbound-child", PARENT_NAME);
                assert_eq!(
                    resolution.status(),
                    if case.expected {
                        RuntimeChannelBindingStatus::Owned
                    } else {
                        RuntimeChannelBindingStatus::Unowned
                    },
                    "{}",
                    case.label
                );
            },
        );
    }
}

#[test]
fn strict_thread_only_signal_can_precede_but_cannot_create_runtime_scope() {
    for (strict_flag, pinned_flag, expected_flag) in [(false, true, false), (true, false, true)] {
        let mut pinned = role_binding("pinned-parent", "claude");
        pinned["channelId"] = json!(PARENT_ID.to_string());
        pinned["workspace"] = json!("/tmp/pinned-parent");
        pinned["threadInherit"] = json!(pinned_flag);
        with_role_map(
            json!({
                "fallbackByChannelName": {"enabled": true},
                "byChannelId": {
                    PARENT_ID.to_string(): {"threadInherit": strict_flag},
                },
                "byChannelName": {PARENT_NAME: pinned},
            }),
            || {
                let strict = settings::resolve_runtime_strict_configured_binding(
                    serenity::ChannelId::new(PARENT_ID),
                )
                .expect("an explicit strict thread flag must survive to composition");
                assert!(!strict.owns_scope());
                assert_eq!(strict.thread_inherit, Some(strict_flag));

                let payload = settings::resolve_runtime_configured_binding(
                    serenity::ChannelId::new(PARENT_ID),
                    Some(PARENT_NAME),
                )
                .expect("the exact pinned binding supplies the valid runtime scope");
                assert!(payload.owns_scope());
                assert_eq!(payload.thread_inherit, expected_flag);
            },
        );
    }

    with_role_map(
        json!({
            "byChannelId": {
                PARENT_ID.to_string(): {"threadInherit": false},
            },
        }),
        || {
            let strict = settings::resolve_runtime_strict_configured_binding(
                serenity::ChannelId::new(PARENT_ID),
            )
            .expect("strict thread-only signal is retained internally");
            assert!(!strict.owns_scope());
            assert!(
                settings::merge_runtime_configured_binding_with_pinned_name(
                    serenity::ChannelId::new(PARENT_ID),
                    Some(PARENT_NAME),
                    Some(strict),
                )
                .is_none(),
                "a thread flag alone cannot establish runtime ownership"
            );
            assert_eq!(
                classify_thread("unbound-child", PARENT_NAME).status(),
                RuntimeChannelBindingStatus::Unowned
            );
        },
    );

    with_role_map(
        json!({
            "fallbackByChannelName": {"enabled": true},
            "byChannelName": {
                PARENT_NAME: {
                    "channelId": PARENT_ID.to_string(),
                    "threadInherit": false,
                },
            },
        }),
        || {
            assert!(
                settings::resolve_runtime_configured_binding(
                    serenity::ChannelId::new(PARENT_ID),
                    Some(PARENT_NAME),
                )
                .is_none(),
                "an exact pinned flag-only entry is not ownership authority"
            );
        },
    );

    for unsafe_pinned in [
        json!({
            "roleId": "unpinned-parent",
            "promptFile": "/tmp/unpinned.md",
            "threadInherit": true,
        }),
        json!({
            "channelId": WRONG_PARENT_ID.to_string(),
            "roleId": "wrong-parent",
            "promptFile": "/tmp/wrong.md",
            "threadInherit": true,
        }),
    ] {
        with_role_map(
            json!({
                "fallbackByChannelName": {"enabled": true},
                "byChannelId": {
                    PARENT_ID.to_string(): {"threadInherit": false},
                },
                "byChannelName": {PARENT_NAME: unsafe_pinned},
            }),
            || {
                assert!(
                    settings::resolve_runtime_configured_binding(
                        serenity::ChannelId::new(PARENT_ID),
                        Some(PARENT_NAME),
                    )
                    .is_none(),
                    "wrong or unpinned names cannot fill a strict thread-only signal"
                );
            },
        );
    }
}

#[test]
fn blank_or_invalid_roles_do_not_own_scope_but_nonblank_workspace_still_does() {
    for invalid in [
        json!({"roleId": "   ", "promptFile": "/tmp/prompt.md"}),
        json!({"roleId": "agent", "promptFile": "   "}),
    ] {
        for direct_kind in [DirectBindingKind::Id, DirectBindingKind::Name] {
            let mut by_id = Map::new();
            let mut by_name = Map::new();
            match direct_kind {
                DirectBindingKind::Id => {
                    by_id.insert(CHILD_ID.to_string(), invalid.clone());
                }
                DirectBindingKind::Name => {
                    let mut invalid = invalid.clone();
                    invalid["channelId"] = json!(CHILD_ID.to_string());
                    by_name.insert(CHILD_NAME.to_string(), invalid);
                }
            }
            with_role_map(
                json!({
                    "fallbackByChannelName": {"enabled": true},
                    "byChannelId": by_id,
                    "byChannelName": by_name,
                }),
                || {
                    assert_eq!(
                        classify_thread(CHILD_NAME, PARENT_NAME).status(),
                        RuntimeChannelBindingStatus::Unowned,
                        "invalid {direct_kind:?} role cannot establish ownership"
                    );
                },
            );
        }
    }

    with_role_map(
        json!({
            "byChannelId": {
                CHILD_ID.to_string(): {
                    "roleId": " ",
                    "promptFile": " ",
                    "workspace": "/tmp/workspace-still-valid",
                },
            },
        }),
        || {
            let resolution = classify_thread(CHILD_NAME, PARENT_NAME);
            let (payload, _) = authority_payload(&resolution);
            assert!(payload.role.is_none());
            assert_eq!(
                payload.workspace.as_deref(),
                Some("/tmp/workspace-still-valid")
            );
        },
    );
}

#[test]
fn strict_id_survives_missing_metadata_while_name_only_fails_closed() {
    with_role_map(
        json!({"byChannelId": {CHILD_ID.to_string(): role_binding("strict", "codex")}}),
        || {
            let payload = settings::resolve_runtime_configured_binding(
                serenity::ChannelId::new(CHILD_ID),
                None,
            )
            .expect("strict ID payload");
            let resolution = classify_runtime_channel_binding_from_captured_metadata(
                RuntimeChannelIdentity::new(serenity::ChannelId::new(CHILD_ID), None),
                Some(payload),
                None,
                RuntimeMetadataState::Unknown,
            );
            assert_eq!(resolution.status(), RuntimeChannelBindingStatus::Owned);
            assert_eq!(
                resolution
                    .authority_identity()
                    .and_then(RuntimeChannelIdentity::channel_name),
                None
            );
        },
    );

    let mut pinned = role_binding("name-only", "claude");
    pinned["channelId"] = json!(CHILD_ID.to_string());
    with_role_map(
        json!({
            "fallbackByChannelName": {"enabled": true},
            "byChannelName": {CHILD_NAME: pinned},
        }),
        || {
            let strict = settings::resolve_runtime_strict_configured_binding(
                serenity::ChannelId::new(CHILD_ID),
            );
            assert!(strict.is_none());
            let direct_payload = settings::merge_runtime_configured_binding_with_pinned_name(
                serenity::ChannelId::new(CHILD_ID),
                None,
                strict,
            );
            let resolution = classify_runtime_channel_binding_from_captured_metadata(
                RuntimeChannelIdentity::new(serenity::ChannelId::new(CHILD_ID), None),
                direct_payload,
                None,
                RuntimeMetadataState::Unknown,
            );
            assert_eq!(resolution.status(), RuntimeChannelBindingStatus::Unknown);
            assert!(resolution.authority().is_none());
        },
    );
}

#[tokio::test]
async fn injected_metadata_resolver_stops_at_a_direct_guild_child() {
    with_role_map_async(
        json!({"byChannelId": {CHILD_ID.to_string(): role_binding("direct-child", "codex")}}),
        |_| async {
            let child_id = serenity::ChannelId::new(CHILD_ID);
            let parent_id = serenity::ChannelId::new(PARENT_ID);
            let lookup = FakeRuntimeChannelMetadataLookup::new([(
                child_id,
                Ok(guild_metadata(CHILD_NAME, true, Some(PARENT_ID))),
            )]);

            let resolution =
                resolve_runtime_channel_binding_resolution_with_lookup(&lookup, child_id).await;
            assert_eq!(resolution.status(), RuntimeChannelBindingStatus::Owned);
            assert!(
                resolution
                    .authority()
                    .is_some_and(RuntimeBindingAuthority::is_direct)
            );
            assert_eq!(lookup.call_count(child_id), 1);
            assert_eq!(lookup.call_count(parent_id), 0);
        },
    )
    .await;
}

#[tokio::test]
async fn injected_metadata_resolver_inherits_a_live_thread_parent_once() {
    with_role_map_async(
        json!({"byChannelId": {PARENT_ID.to_string(): role_binding("strict-parent", "codex")}}),
        |_| async {
            let child_id = serenity::ChannelId::new(CHILD_ID);
            let parent_id = serenity::ChannelId::new(PARENT_ID);
            let lookup = FakeRuntimeChannelMetadataLookup::new([
                (
                    child_id,
                    Ok(guild_metadata(CHILD_NAME, true, Some(PARENT_ID))),
                ),
                (parent_id, Ok(guild_metadata(PARENT_NAME, false, None))),
            ]);

            let resolution =
                resolve_runtime_channel_binding_resolution_with_lookup(&lookup, child_id).await;
            assert_eq!(resolution.status(), RuntimeChannelBindingStatus::Owned);
            assert!(
                resolution
                    .authority()
                    .is_some_and(RuntimeBindingAuthority::is_inherited_parent)
            );
            assert_eq!(lookup.call_count(child_id), 1);
            assert_eq!(lookup.call_count(parent_id), 1);
        },
    )
    .await;
}

#[tokio::test]
async fn injected_metadata_resolver_keeps_strict_parent_on_parent_http_failure() {
    with_role_map_async(
        json!({"byChannelId": {PARENT_ID.to_string(): role_binding("strict-parent", "codex")}}),
        |_| async {
            let child_id = serenity::ChannelId::new(CHILD_ID);
            let parent_id = serenity::ChannelId::new(PARENT_ID);
            let lookup = FakeRuntimeChannelMetadataLookup::new([
                (
                    child_id,
                    Ok(guild_metadata(CHILD_NAME, true, Some(PARENT_ID))),
                ),
                (parent_id, Err(())),
            ]);

            let resolution =
                resolve_runtime_channel_binding_resolution_with_lookup(&lookup, child_id).await;
            assert_eq!(resolution.status(), RuntimeChannelBindingStatus::Owned);
            assert_eq!(resolution.authority_channel_id(), parent_id);
            assert_eq!(
                resolution
                    .authority_identity()
                    .and_then(RuntimeChannelIdentity::channel_name),
                None
            );
            assert_eq!(lookup.call_count(child_id), 1);
            assert_eq!(lookup.call_count(parent_id), 1);
        },
    )
    .await;
}

#[tokio::test]
async fn injected_metadata_resolver_child_http_failure_distinguishes_strict_from_name_only() {
    with_role_map_async(
        json!({"byChannelId": {CHILD_ID.to_string(): role_binding("strict-child", "codex")}}),
        |_| async {
            let child_id = serenity::ChannelId::new(CHILD_ID);
            let lookup = FakeRuntimeChannelMetadataLookup::new([(child_id, Err(()))]);
            let resolution =
                resolve_runtime_channel_binding_resolution_with_lookup(&lookup, child_id).await;
            assert_eq!(resolution.status(), RuntimeChannelBindingStatus::Owned);
            assert_eq!(lookup.call_count(child_id), 1);
        },
    )
    .await;

    let mut pinned = role_binding("name-only-child", "claude");
    pinned["channelId"] = json!(CHILD_ID.to_string());
    with_role_map_async(
        json!({
            "fallbackByChannelName": {"enabled": true},
            "byChannelName": {CHILD_NAME: pinned},
        }),
        |_| async {
            let child_id = serenity::ChannelId::new(CHILD_ID);
            let lookup = FakeRuntimeChannelMetadataLookup::new([(child_id, Err(()))]);
            let resolution =
                resolve_runtime_channel_binding_resolution_with_lookup(&lookup, child_id).await;
            assert_eq!(resolution.status(), RuntimeChannelBindingStatus::Unknown);
            assert!(resolution.authority().is_none());
            assert_eq!(lookup.call_count(child_id), 1);
        },
    )
    .await;
}

#[tokio::test]
async fn injected_metadata_resolver_handles_private_dm_and_missing_thread_parent() {
    with_role_map_async(json!({}), |_| async {
        let child_id = serenity::ChannelId::new(CHILD_ID);
        let dm_lookup = FakeRuntimeChannelMetadataLookup::new([(
            child_id,
            Ok(RuntimeChannelMetadata::DirectMessage),
        )]);
        let dm_resolution =
            resolve_runtime_channel_binding_resolution_with_lookup(&dm_lookup, child_id).await;
        assert_eq!(dm_resolution.status(), RuntimeChannelBindingStatus::Owned);
        assert!(
            dm_resolution
                .authority()
                .is_some_and(RuntimeBindingAuthority::is_direct)
        );
        assert_eq!(dm_lookup.call_count(child_id), 1);

        let missing_parent_lookup = FakeRuntimeChannelMetadataLookup::new([(
            child_id,
            Ok(guild_metadata(CHILD_NAME, true, None)),
        )]);
        let missing_parent_resolution = resolve_runtime_channel_binding_resolution_with_lookup(
            &missing_parent_lookup,
            child_id,
        )
        .await;
        assert_eq!(
            missing_parent_resolution.status(),
            RuntimeChannelBindingStatus::Unowned
        );
        assert!(missing_parent_resolution.parent().is_none());
        assert_eq!(missing_parent_lookup.call_count(child_id), 1);
        assert_eq!(
            missing_parent_lookup.call_count(serenity::ChannelId::new(PARENT_ID)),
            0
        );
    })
    .await;
}

#[tokio::test]
async fn injected_metadata_resolver_never_reloads_strict_child_after_lookup() {
    with_role_map_async(
        json!({"byChannelId": {CHILD_ID.to_string(): role_binding("captured-child", "codex")}}),
        |role_map_path| async move {
            let child_id = serenity::ChannelId::new(CHILD_ID);
            let mutation_path = role_map_path.clone();
            let lookup = FakeRuntimeChannelMetadataLookup::new([(
                child_id,
                Ok(guild_metadata(CHILD_NAME, false, None)),
            )])
            .mutate_during_lookup(child_id, move || {
                let mut pinned = json!({
                    "channelId": CHILD_ID.to_string(),
                    "workspace": "/tmp/pinned-after-child-lookup",
                });
                pinned["threadInherit"] = json!(false);
                fs::write(
                    mutation_path,
                    json!({
                        "fallbackByChannelName": {"enabled": true},
                        "byChannelId": {CHILD_ID.to_string(): role_binding("late-child", "claude")},
                        "byChannelName": {CHILD_NAME: pinned},
                    })
                    .to_string(),
                )
                .expect("mutate config during child metadata lookup");
            });

            let resolution =
                resolve_runtime_channel_binding_resolution_with_lookup(&lookup, child_id).await;
            let (payload, direct) = authority_payload(&resolution);
            assert!(direct);
            assert_eq!(
                payload.role.as_ref().map(|role| role.role_id.as_str()),
                Some("captured-child")
            );
            assert_eq!(
                payload.workspace.as_deref(),
                Some("/tmp/pinned-after-child-lookup")
            );
            assert!(!payload.thread_inherit);
            assert_eq!(lookup.call_count(child_id), 1);
        },
    )
    .await;
}

#[tokio::test]
async fn injected_metadata_resolver_never_reloads_strict_parent_after_lookup() {
    with_role_map_async(
        json!({"byChannelId": {PARENT_ID.to_string(): role_binding("captured-parent", "codex")}}),
        |role_map_path| async move {
            let child_id = serenity::ChannelId::new(CHILD_ID);
            let parent_id = serenity::ChannelId::new(PARENT_ID);
            let mutation_path = role_map_path.clone();
            let lookup = FakeRuntimeChannelMetadataLookup::new([
                (
                    child_id,
                    Ok(guild_metadata(CHILD_NAME, true, Some(PARENT_ID))),
                ),
                (
                    parent_id,
                    Ok(guild_metadata(PARENT_NAME, false, None)),
                ),
            ])
            .mutate_during_lookup(parent_id, move || {
                let pinned = json!({
                    "channelId": PARENT_ID.to_string(),
                    "workspace": "/tmp/pinned-after-parent-lookup",
                });
                fs::write(
                    mutation_path,
                    json!({
                        "fallbackByChannelName": {"enabled": true},
                        "byChannelId": {PARENT_ID.to_string(): role_binding("late-parent", "claude")},
                        "byChannelName": {PARENT_NAME: pinned},
                    })
                    .to_string(),
                )
                .expect("mutate config during parent metadata lookup");
            });

            let resolution =
                resolve_runtime_channel_binding_resolution_with_lookup(&lookup, child_id).await;
            let (payload, direct) = authority_payload(&resolution);
            assert!(!direct);
            assert_eq!(
                payload.role.as_ref().map(|role| role.role_id.as_str()),
                Some("captured-parent")
            );
            assert_eq!(
                payload.workspace.as_deref(),
                Some("/tmp/pinned-after-parent-lookup")
            );
            assert_eq!(lookup.call_count(child_id), 1);
            assert_eq!(lookup.call_count(parent_id), 1);
        },
    )
    .await;
}

#[test]
fn private_channel_is_direct_owned_without_a_config_payload() {
    let resolution = RuntimeChannelBindingResolution::direct_message(serenity::ChannelId::new(7));
    assert_eq!(resolution.status(), RuntimeChannelBindingStatus::Owned);
    let (payload, direct) = authority_payload(&resolution);
    assert!(direct);
    assert!(!payload.owns_scope());
}

#[test]
fn authority_identity_is_structurally_coupled_to_every_owned_variant() {
    with_role_map(
        json!({
            "byChannelId": {
                CHILD_ID.to_string(): role_binding("child", "claude"),
                PARENT_ID.to_string(): role_binding("parent", "codex"),
            },
        }),
        || {
            let direct = classify_thread(CHILD_NAME, PARENT_NAME);
            assert!(
                direct
                    .authority()
                    .is_some_and(RuntimeBindingAuthority::is_direct)
            );
            assert_eq!(
                direct
                    .authority_identity()
                    .expect("direct identity")
                    .channel_id(),
                serenity::ChannelId::new(CHILD_ID)
            );
        },
    );
    with_role_map(
        json!({"byChannelId": {PARENT_ID.to_string(): role_binding("parent", "codex")}}),
        || {
            let inherited = classify_thread("unbound-child", PARENT_NAME);
            assert!(
                inherited
                    .authority()
                    .is_some_and(RuntimeBindingAuthority::is_inherited_parent)
            );
            assert_eq!(
                inherited
                    .authority_identity()
                    .expect("inherited identity")
                    .channel_id(),
                serenity::ChannelId::new(PARENT_ID)
            );
        },
    );

    let source = include_str!("../channel_routing.rs");
    assert!(source.contains(
        "Direct {\n        identity: RuntimeChannelIdentity,\n        payload: settings::ConfiguredBindingPayload,"
    ));
    assert!(source.contains(
        "InheritedParent {\n        identity: RuntimeChannelIdentity,\n        payload: settings::ConfiguredBindingPayload,"
    ));
    assert!(!source.contains("pub(in crate::services::discord) authority:"));
}

#[test]
fn inherited_name_only_parent_role_and_workspace_bindings_are_owned() {
    for parent_binding in [
        {
            let mut binding = role_binding("project-agentdesk", "codex");
            binding["channelId"] = json!(PARENT_ID.to_string());
            binding
        },
        json!({
            "channelId": PARENT_ID.to_string(),
            "workspace": "/tmp/name-only-parent-workspace",
        }),
    ] {
        with_role_map(
            json!({
                "fallbackByChannelName": {"enabled": true},
                "byChannelName": {PARENT_NAME: parent_binding},
            }),
            || {
                let resolution = classify_thread("unbound-child", PARENT_NAME);
                assert_eq!(
                    resolution.status(),
                    RuntimeChannelBindingStatus::Owned,
                    "an unbound child must inherit a live-name-only parent binding"
                );
                assert_eq!(
                    resolution.authority_channel_id(),
                    serenity::ChannelId::new(PARENT_ID),
                );
                assert!(missing_mention_is_skipped(&resolution, &[PARENT_ID]));
                assert!(
                    !missing_mention_is_skipped(&resolution, &[CHILD_ID]),
                    "an inherited child uses the parent's mention policy"
                );
            },
        );
    }
}

#[test]
fn pinned_name_workspace_requires_fallback_exact_id_and_nonblank_payload() {
    let pinned_workspace = json!({
        "channelId": CHILD_ID.to_string(),
        "workspace": "/tmp/pinned-child-workspace",
    });
    with_role_map(
        json!({
            "fallbackByChannelName": {"enabled": true},
            "byChannelName": {CHILD_NAME: pinned_workspace.clone()},
        }),
        || {
            let resolution = classify_thread(CHILD_NAME, "unbound-parent");
            assert_eq!(resolution.status(), RuntimeChannelBindingStatus::Owned);
            assert_eq!(
                resolution.authority_channel_id(),
                serenity::ChannelId::new(CHILD_ID)
            );
        },
    );
    with_role_map(
        json!({
            "fallbackByChannelName": {"enabled": false},
            "byChannelName": {CHILD_NAME: pinned_workspace},
        }),
        || {
            assert_eq!(
                classify_thread(CHILD_NAME, "unbound-parent").status(),
                RuntimeChannelBindingStatus::Unowned,
                "fallback-disabled by-name entries cannot become ownership authority"
            );
        },
    );
    with_role_map(
        json!({
            "fallbackByChannelName": {"enabled": true},
            "byChannelName": {
                CHILD_NAME: {
                    "channelId": CHILD_ID.to_string(),
                    "workspace": "   ",
                },
            },
        }),
        || {
            assert_eq!(
                classify_thread(CHILD_NAME, "unbound-parent").status(),
                RuntimeChannelBindingStatus::Unowned,
                "blank workspace is not a binding payload"
            );
        },
    );
}

#[test]
fn unpinned_role_map_name_does_not_grant_runtime_ownership() {
    with_role_map(
        json!({
            "fallbackByChannelName": {"enabled": true},
            "byChannelName": {
                PARENT_NAME: role_binding("project-agentdesk", "codex"),
            },
        }),
        || {
            assert_eq!(
                classify_thread("unbound-child", PARENT_NAME).status(),
                RuntimeChannelBindingStatus::Unowned,
                "same-name fallback without an exact channelId is not ownership authority"
            );
        },
    );
}

#[test]
fn unpinned_org_name_does_not_grant_runtime_ownership() {
    let temp = TempDir::new().expect("temporary AgentDesk root");
    let root = temp.path().join(".adk");
    let config = root.join("config");
    fs::create_dir_all(&config).expect("create config directory");
    fs::write(
        config.join("org.yaml"),
        format!(
            r#"version: 1
agents:
  project-agentdesk:
    display_name: AgentDesk
    provider: codex
    workspace: /tmp/agentdesk
channels:
  by_name:
    enabled: true
    mappings:
      {PARENT_NAME}:
        agent: project-agentdesk
"#
        ),
    )
    .expect("write org schema");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", &root);
    assert_eq!(
        classify_thread("unbound-child", PARENT_NAME).status(),
        RuntimeChannelBindingStatus::Unowned,
        "ORG by_name fallback is not ID-pinned ownership authority"
    );
}

#[test]
fn agentdesk_name_alias_without_exact_id_does_not_grant_runtime_ownership() {
    let temp = TempDir::new().expect("temporary AgentDesk root");
    let root = temp.path().join(".adk");
    let config = root.join("config");
    fs::create_dir_all(&config).expect("create config directory");
    fs::write(
        config.join("agentdesk.yaml"),
        format!(
            r#"server:
  port: 8791
agents:
  - id: alias-agent
    name: Alias Agent
    provider: codex
    channels:
      codex:
        id: "{WRONG_PARENT_ID}"
        name: "{CHILD_NAME}"
        prompt_file: "/tmp/alias.md"
        workspace: "/tmp/alias"
"#
        ),
    )
    .expect("write AgentDesk config");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", &root);
    let resolution = classify_thread(CHILD_NAME, "unbound-parent");
    assert_eq!(
        resolution.status(),
        RuntimeChannelBindingStatus::Unowned,
        "AgentDesk name aliases are not exact runtime ownership"
    );
}

#[test]
fn wrong_live_names_and_mismatched_ids_do_not_overgrant_name_only_bindings() {
    let mut child = role_binding("review-agent", "claude");
    child["channelId"] = json!(CHILD_ID.to_string());
    let mut parent = role_binding("project-agentdesk", "codex");
    parent["channelId"] = json!(PARENT_ID.to_string());
    with_role_map(
        json!({
            "fallbackByChannelName": {"enabled": true},
            "byChannelName": {
                CHILD_NAME: child,
                PARENT_NAME: parent,
            },
        }),
        || {
            assert_eq!(
                classify_thread("wrong-child-name", "wrong-parent-name").status(),
                RuntimeChannelBindingStatus::Unowned,
            );
        },
    );

    let mut mismatched_parent = role_binding("project-agentdesk", "codex");
    mismatched_parent["channelId"] = json!(WRONG_PARENT_ID.to_string());
    with_role_map(
        json!({
            "fallbackByChannelName": {"enabled": true},
            "byChannelName": {PARENT_NAME: mismatched_parent},
        }),
        || {
            assert_eq!(
                classify_thread("unbound-child", PARENT_NAME).status(),
                RuntimeChannelBindingStatus::Unowned,
                "a by-name entry pinned to another channel must not grant ownership"
            );
        },
    );
}

#[test]
fn intake_mention_guard_uses_runtime_binding_authority() {
    let source = include_str!("../../router/intake_gate.rs");
    assert_eq!(
        source
            .matches("&settings_snapshot,\n                    mention_authority_channel_id,")
            .count(),
        1,
        "mention policy must use the same direct-child-first binding authority"
    );
    assert!(!source.contains(
        "&settings_snapshot,\n                    effective_channel_id,\n                    is_dm,"
    ));
}
