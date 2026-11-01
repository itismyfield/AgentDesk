use std::collections::BTreeSet;

use serde::de::{self, DeserializeOwned, Visitor};

use super::*;
use crate::config::Config;

/// Captures the serde field names a derived `Deserialize` asks for, without values.
struct FieldProbe<'a>(&'a mut Option<&'static [&'static str]>);

impl<'de> de::Deserializer<'de> for FieldProbe<'_> {
    type Error = de::value::Error;

    fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
        Err(de::Error::custom("not a struct"))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        fields: &'static [&'static str],
        _: V,
    ) -> Result<V::Value, Self::Error> {
        *self.0 = Some(fields);
        Err(de::Error::custom("probed"))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes
        byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map enum
        identifier ignored_any
    }
}

/// Field names of `T`, without camelCase aliases of a snake_case field.
fn fields_of<T: DeserializeOwned>() -> Vec<&'static str> {
    let mut fields = None;
    let _ = T::deserialize(FieldProbe(&mut fields));
    let fields = fields.unwrap_or_else(|| panic!("{} is not a struct", std::any::type_name::<T>()));
    let snake = |name: &str| {
        name.chars().fold(String::new(), |mut out, c| {
            if c.is_ascii_uppercase() {
                out.push('_');
                out.push(c.to_ascii_lowercase());
            } else {
                out.push(c);
            }
            out
        })
    };
    fields
        .iter()
        .copied()
        .filter(|name| {
            !(name.chars().any(|c| c.is_ascii_uppercase())
                && fields.contains(&snake(name).as_str()))
        })
        .collect()
}

fn struct_registry() -> Vec<(&'static str, Vec<&'static str>)> {
    use crate::config::*;
    vec![
        ("", fields_of::<Config>()),
        ("server", fields_of::<ServerConfig>()),
        ("discord", fields_of::<DiscordConfig>()),
        ("discord.bots.*", fields_of::<BotConfig>()),
        ("discord.bots.*.auth", fields_of::<DiscordBotAuthConfig>()),
        ("providers.*", fields_of::<ProviderConfig>()),
        ("mcp_servers.*", fields_of::<McpServerConfig>()),
        ("agents.*", fields_of::<AgentDef>()),
        ("agents.*.channels.*", fields_of::<AgentChannelConfig>()),
        ("github", fields_of::<GitHubConfig>()),
        ("policies", fields_of::<PoliciesConfig>()),
        ("data", fields_of::<DataConfig>()),
        ("database", fields_of::<DatabaseConfig>()),
        ("cluster", fields_of::<ClusterConfig>()),
        (
            "cluster.gateway_handback_breaker",
            fields_of::<runtime_profile::GatewayHandbackBreakerConfig>(),
        ),
        ("cluster.nodes.*", fields_of::<ClusterNodeConfig>()),
        (
            "cluster.blackout_windows.*.*",
            fields_of::<ClusterBlackoutWindowConfig>(),
        ),
        (
            "cluster.dispatch_routing",
            fields_of::<ClusterDispatchRoutingConfig>(),
        ),
        (
            "cluster.intake_routing",
            fields_of::<ClusterIntakeRoutingConfig>(),
        ),
        (
            "cluster.semaphores.*",
            fields_of::<ClusterSemaphoreConfig>(),
        ),
        ("kanban", fields_of::<KanbanConfig>()),
        ("review", fields_of::<ReviewConfig>()),
        ("placeholder", fields_of::<PlaceholderConfig>()),
        ("runtime", fields_of::<RuntimeSettingsConfig>()),
        ("routines", fields_of::<RoutinesConfig>()),
        ("onboarding", fields_of::<OnboardingConfig>()),
        ("memory", fields_of::<MemoryConfig>()),
        ("memory.file", fields_of::<FileMemoryConfig>()),
        ("memory.mcp", fields_of::<McpMemoryConfig>()),
        ("mcp", fields_of::<McpConfig>()),
        (
            "prompt_manifest_retention",
            fields_of::<PromptManifestRetentionConfig>(),
        ),
        (
            "tui_o",
            fields_of::<crate::services::tui_o::shadow::tap::TuiOConfig>(),
        ),
        (
            "tui_o.writer",
            fields_of::<crate::services::tui_o::shadow::tap::WriterChannelsConfig>(),
        ),
        (
            "tui_o.tool_cards",
            fields_of::<crate::services::tui_o::shadow::tap::ToolCardsConfig>(),
        ),
        (
            "tui_o.turn",
            fields_of::<crate::services::tui_o::turn_mode::TurnConfig>(),
        ),
        ("session_hosts", fields_of::<SessionHostsConfig>()),
    ]
}

/// serde lists aliases among field names; camelCase ones are dropped in `fields_of`.
const ALIASES: &[&str] = &["agents.*.channels.*.channel_name"];

/// The struct registry introspects `path` itself or something below it.
fn expanded(prefixes: &BTreeSet<&str>, path: &str) -> bool {
    prefixes
        .iter()
        .any(|prefix| *prefix == path || is_ancestor(path, prefix))
}

fn is_ancestor(ancestor: &str, path: &str) -> bool {
    path.len() > ancestor.len()
        && path.starts_with(ancestor)
        && path.as_bytes()[ancestor.len()] == b'.'
}

/// Every struct path registered here is a subtree; its own fields must each be classified.
#[test]
fn every_config_field_is_classified_exactly_once() {
    let structs = struct_registry();
    let prefixes: BTreeSet<&str> = structs.iter().map(|(prefix, _)| *prefix).collect();
    let mut leaves = Vec::new();
    for (prefix, fields) in &structs {
        for field in fields {
            let path = if prefix.is_empty() {
                field.to_string()
            } else {
                format!("{prefix}.{field}")
            };
            if !expanded(&prefixes, &path) && !ALIASES.contains(&path.as_str()) {
                leaves.push(path);
            }
        }
    }
    let rows: Vec<&str> = FIELDS.iter().map(|field| field.path).collect();
    let mut problems = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if rows[..i].contains(row) {
            problems.push(format!("duplicate row {row}"));
        }
        if let Some(outer) = rows.iter().find(|other| is_ancestor(other, row)) {
            problems.push(format!("{row} overlaps {outer}"));
        }
        let covers = leaves
            .iter()
            .any(|leaf| leaf == row || is_ancestor(row, leaf));
        if !covers {
            problems.push(format!("{row} names no config field"));
        }
        let subtree = expanded(&prefixes, row);
        let field = &FIELDS[i];
        if subtree && field.class != ReloadClass::Restart {
            problems.push(format!(
                "{row} is a whole subtree but {}",
                field.class.as_str()
            ));
        }
    }
    for leaf in &leaves {
        let covering = rows
            .iter()
            .filter(|row| *row == leaf || is_ancestor(row, leaf))
            .count();
        if covering != 1 {
            problems.push(format!("{leaf} is covered by {covering} rows"));
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn file_rows_name_tracked_files() {
    for file in FILES {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file.path);
        assert!(path.is_file(), "{}", file.path);
        assert_ne!(file.class, ReloadClass::Live, "{}", file.path);
    }
}

#[test]
fn reader_columns_name_existing_files() {
    for field in FIELDS.iter().chain(FILES) {
        let file = field.reader.split_whitespace().next().unwrap_or("");
        let file = file.trim_matches('`');
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
        assert!(
            path.is_file(),
            "{}: reader {} is not a file",
            field.path,
            field.reader
        );
    }
}

/// The committed `docs/config-reload-classes.md`.
fn render_markdown() -> String {
    let mut out = String::from(
        "# Config Reload Classes\n\n\
         <!-- Generated from src/config/reload_classes/table.rs; do not edit by hand. -->\n\
         <!-- Regenerate: ADK_WRITE_RELOAD_CLASSES_DOC=1 cargo test --lib config::reload_classes:: -->\n\n\
         When an edit to `agentdesk.yaml` takes effect. Config hot reload stores every\n\
         valid edit in the live snapshot, but only `live` fields act on it. A change to a\n\
         `restart` or `seeded-at-boot` field logs one WARN line naming the field.\n\n\
         Daemon snapshot readers retain the last validated boot/reload value when hot reload\n\
         is disabled or its watcher cannot start. The bot or standalone server owns the watcher;\n\
         ending the HTTP task does not stop it. The config directory is polled every two\n\
         seconds with content comparison, independently of native filesystem notifications.\n\n\
         - `live`: read from the live snapshot (or disk) at use time.\n\
         - `restart`: captured at boot or on first access; restart to apply.\n\
         - `seeded-at-boot`: copied into the DB/kv at boot, and runtime readers read that\n  \
           copy; restart to re-seed.\n\n\
         `*` stands for any map key or list item.\n\n\
         | Field | Class | Read or captured by |\n|---|---|---|\n",
    );
    for field in FIELDS {
        out.push_str(&format!(
            "| `{}` | {} | {} |\n",
            field.path,
            field.class.as_str(),
            field.reader
        ));
    }
    out.push_str(
        "\n## Files outside `agentdesk.yaml`\n\n\
         | File | Class | Read or captured by |\n|---|---|---|\n",
    );
    for file in FILES {
        out.push_str(&format!(
            "| `{}` | {} | {} |\n",
            file.path,
            file.class.as_str(),
            file.reader
        ));
    }
    out
}

#[test]
fn generated_doc_matches_the_table() {
    let doc =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/config-reload-classes.md");
    let rendered = render_markdown();
    if std::env::var_os("ADK_WRITE_RELOAD_CLASSES_DOC").is_some() {
        std::fs::write(&doc, &rendered).unwrap();
    }
    let committed = std::fs::read_to_string(&doc).unwrap_or_default();
    assert!(
        committed == rendered,
        "docs/config-reload-classes.md is stale; regenerate with \
         ADK_WRITE_RELOAD_CLASSES_DOC=1 cargo test --lib config::reload_classes::"
    );
}

fn class(path: &str) -> ReloadClass {
    class_of(path).unwrap_or_else(|| panic!("{path} has no row"))
}

// The over- and under-reported cases from the audit, as the code resolves them.
#[test]
fn audited_fields_have_their_traced_class() {
    use ReloadClass::{Live, Restart, SeededAtBoot};
    for (path, expected) in [
        // `cluster` is no longer restart as a whole.
        ("cluster.intake_routing.capacity_aware", Live),
        ("cluster.intake_routing.owner_authority_channel_ids", Live),
        ("cluster.dispatch_routing.opt_out_dispatch_types", Live),
        ("cluster.gateway_handback_breaker.window_secs", Live),
        ("cluster.semaphores.*.capacity", Live),
        // The intake worker consumer is spawned only at bot boot.
        ("cluster.intake_routing.mode", Restart),
        // Wait-queue and outbox loops hold boot copies of the routing config.
        ("cluster.dispatch_routing.default_preferred_labels", Restart),
        ("cluster.dispatch_routing.wake_interval_secs", Restart),
        ("cluster.execution_slots", Restart),
        // Captured by the hook registry's LazyLock on first access.
        ("runtime.tui_hook_buffer_ttl_secs", Restart),
        ("runtime.tui_unclaimed_stop_delay_ms", Restart),
        ("runtime.tui_hook_registry_enabled", Live),
        ("policies/default-pipeline.yaml", Restart),
        // Copied into kv_meta / the runtime-config blob or the DB at boot.
        ("review.enabled", SeededAtBoot),
        ("review.max_rounds", SeededAtBoot),
        ("kanban.manager_channel_id", SeededAtBoot),
        ("runtime.max_entry_retries", SeededAtBoot),
        ("runtime.requested_timeout_min", SeededAtBoot),
        ("github.repos", SeededAtBoot),
        ("agents.*.id", SeededAtBoot),
        ("agents.*.name", SeededAtBoot),
        ("agents.*.channels.*.runtime", Restart),
        ("agents.*.channels.*.model", Live),
    ] {
        assert_eq!(class(path), expected, "{path}");
    }
}

#[test]
fn changes_are_reported_per_field_and_skip_live_fields() {
    let base = Config::default();
    let changed = |edit: fn(&mut Config)| {
        let mut new = base.clone();
        edit(&mut new);
        changed_non_live(&base, &new)
            .into_iter()
            .map(|field| field.path)
            .collect::<Vec<_>>()
    };
    assert!(changed(|c| c.cluster.intake_routing.capacity_aware ^= true).is_empty());
    assert!(changed(|c| c.runtime.tui_hook_registry_enabled = Some(false)).is_empty());
    assert_eq!(
        changed(|c| c.cluster.execution_slots = Some(3)),
        ["cluster.execution_slots"]
    );
    assert_eq!(
        changed(|c| c.runtime.tui_hook_buffer_ttl_secs = Some(90)),
        ["runtime.tui_hook_buffer_ttl_secs"]
    );
    assert_eq!(
        changed(|c| c.review.enabled = Some(false)),
        ["review.enabled"]
    );
    assert_eq!(
        changed(|c| c.github.repos.push("o/r".into())),
        ["github.repos"]
    );
    assert_eq!(
        changed(|c| c.server.browser_guard = crate::config::BrowserGuardMode::Enforce),
        ["server.browser_guard"]
    );
    // A section written out at its default compares equal to an absent one.
    assert!(changed(|c| c.tui_o = Some(Default::default())).is_empty());
}

#[test]
fn a_legacy_string_channel_change_is_reported() {
    let mut old = Config::default();
    let mut agent: crate::config::AgentDef =
        serde_yaml::from_str("{id: a, name: A, provider: claude}").unwrap();
    agent
        .channels
        .insert("claude", crate::config::AgentChannel::Legacy("111".into()));
    old.agents.push(agent);
    let mut new = old.clone();
    new.agents[0]
        .channels
        .insert("claude", crate::config::AgentChannel::Legacy("222".into()));
    let paths: Vec<_> = changed_non_live(&old, &new)
        .into_iter()
        .map(|f| f.path)
        .collect();
    assert!(paths.contains(&"agents.*.channels.*.id"), "{paths:?}");
    assert!(paths.contains(&"agents.*.channels.*.runtime"), "{paths:?}");
}
