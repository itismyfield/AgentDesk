use serde_json::Value;

use crate::config::ClusterConfig;

fn normalize_api_base_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn resolve_worker_api_base_url(config: &ClusterConfig) -> Option<String> {
    if let Some(raw) = config.api_base_url.as_deref() {
        return normalize_api_base_url(raw);
    }
    std::env::var("AGENTDESK_CLUSTER_API_BASE_URL")
        .ok()
        .as_deref()
        .and_then(normalize_api_base_url)
}

pub(crate) fn cluster_capabilities_with_worker_api(config: &ClusterConfig) -> Value {
    let mut capabilities = config.capabilities.clone();
    if let Some(api_base_url) = resolve_worker_api_base_url(config) {
        let mut metadata = capabilities
            .remove("agentdesk_api")
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        metadata.insert("base_url".to_string(), Value::String(api_base_url));
        metadata.insert("session_forwarding".to_string(), Value::Bool(true));
        capabilities.insert("agentdesk_api".to_string(), Value::Object(metadata));
    }
    Value::Object(capabilities)
}

pub(crate) fn worker_api_base_url_from_capabilities(capabilities: &Value) -> Option<String> {
    capabilities
        .get("agentdesk_api")
        .and_then(|metadata| {
            metadata
                .get("base_url")
                .or_else(|| metadata.get("url"))
                .and_then(|value| value.as_str())
        })
        .and_then(normalize_api_base_url)
}

pub(crate) fn session_owner_routing_status(
    owner_instance_id: Option<&str>,
    local_instance_id: Option<&str>,
    worker_nodes: &[Value],
) -> Value {
    let owner_instance_id = owner_instance_id
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let local_instance_id = local_instance_id
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let is_local = match (owner_instance_id, local_instance_id) {
        (Some(owner), Some(local)) => owner == local,
        (Some(_), None) => false,
        (None, _) => false,
    };
    let scope = if owner_instance_id.is_none() {
        "unknown_owner"
    } else if is_local {
        "local"
    } else {
        "foreign"
    };

    let node = owner_instance_id.and_then(|owner| {
        worker_nodes
            .iter()
            .find(|node| node.get("instance_id").and_then(|value| value.as_str()) == Some(owner))
    });
    let node_status = node
        .and_then(|node| node.get("status"))
        .and_then(|value| value.as_str());
    let api_base_url = node
        .and_then(|node| node.get("api_base_url").and_then(|value| value.as_str()))
        .map(str::to_string)
        .or_else(|| {
            node.and_then(|node| node.get("capabilities"))
                .and_then(worker_api_base_url_from_capabilities)
        });

    let routable = !is_local && node_status == Some("online") && api_base_url.is_some();
    let reason = if owner_instance_id.is_none() {
        Some("session_owner_missing")
    } else if is_local {
        None
    } else if node.is_none() {
        Some("worker_node_missing")
    } else if node_status != Some("online") {
        Some("worker_node_stale")
    } else if api_base_url.is_none() {
        Some("worker_api_base_url_missing")
    } else {
        None
    };

    serde_json::json!({
        "instance_id": owner_instance_id,
        "scope": scope,
        "is_local": is_local,
        "node_status": node_status,
        "api_base_url": api_base_url,
        "routable": routable,
        "reason": reason,
    })
}

pub(crate) fn enrich_session_owner_routing(
    sessions: &mut [Value],
    local_instance_id: Option<&str>,
    worker_nodes: &[Value],
) {
    for session in sessions {
        let owner_instance_id = session
            .get("instance_id")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let owner = session_owner_routing_status(
            owner_instance_id.as_deref(),
            local_instance_id,
            worker_nodes,
        );
        if let Some(obj) = session.as_object_mut() {
            obj.insert("owner".to_string(), owner);
        }
    }
}

pub(crate) fn attach_active_session_counts_to_worker_nodes(
    worker_nodes: &mut [Value],
    sessions: &[Value],
) {
    let mut counts = std::collections::BTreeMap::<String, i64>::new();
    for session in sessions {
        if let Some(instance_id) = session
            .get("instance_id")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            *counts.entry(instance_id.to_string()).or_default() += 1;
        }
    }

    for node in worker_nodes {
        let instance_id = node
            .get("instance_id")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let active_session_count = instance_id
            .as_deref()
            .and_then(|instance_id| counts.get(instance_id))
            .copied()
            .unwrap_or(0);
        if let Some(obj) = node.as_object_mut() {
            obj.insert(
                "active_session_count".to_string(),
                serde_json::json!(active_session_count),
            );
        }
    }
}

pub(crate) fn summarize_session_owner_routing(sessions: &[Value]) -> Value {
    let mut local = 0_i64;
    let mut foreign = 0_i64;
    let mut unknown_owner = 0_i64;
    let mut orphaned = 0_i64;
    let mut routable = 0_i64;
    let mut unroutable = 0_i64;
    let mut reasons = std::collections::BTreeMap::<String, i64>::new();

    for session in sessions {
        let Some(owner) = session.get("owner") else {
            continue;
        };
        match owner.get("scope").and_then(|value| value.as_str()) {
            Some("local") => local += 1,
            Some("foreign") => foreign += 1,
            Some("unknown_owner") => unknown_owner += 1,
            _ => {}
        }
        if owner.get("routable").and_then(|value| value.as_bool()) == Some(true) {
            routable += 1;
        } else if owner.get("is_local").and_then(|value| value.as_bool()) != Some(true) {
            unroutable += 1;
        }
        if let Some(reason) = owner
            .get("reason")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
        {
            if matches!(reason, "session_owner_missing" | "worker_node_missing") {
                orphaned += 1;
            }
            *reasons.entry(reason.to_string()).or_default() += 1;
        }
    }

    serde_json::json!({
        "total_active_sessions": sessions.len(),
        "local": local,
        "foreign": foreign,
        "unknown_owner": unknown_owner,
        "orphaned": orphaned,
        "routable": routable,
        "unroutable": unroutable,
        "reasons": reasons,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::{
        attach_active_session_counts_to_worker_nodes, cluster_capabilities_with_worker_api,
        session_owner_routing_status, summarize_session_owner_routing,
        worker_api_base_url_from_capabilities,
    };
    use crate::config::ClusterConfig;
    use serde_json::json;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvRestore {
        previous: Option<String>,
    }

    impl EnvRestore {
        fn new() -> Self {
            Self {
                previous: std::env::var("AGENTDESK_CLUSTER_API_BASE_URL").ok(),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.previous.as_deref() {
                Some(value) => unsafe {
                    std::env::set_var("AGENTDESK_CLUSTER_API_BASE_URL", value)
                },
                None => unsafe { std::env::remove_var("AGENTDESK_CLUSTER_API_BASE_URL") },
            }
        }
    }

    #[test]
    fn configured_api_base_url_is_published_as_worker_metadata_and_validates_scheme() {
        let config = ClusterConfig {
            api_base_url: Some(" http://mac-book.local:8791/ ".to_string()),
            ..ClusterConfig::default()
        };
        let capabilities = cluster_capabilities_with_worker_api(&config);

        assert_eq!(
            worker_api_base_url_from_capabilities(&capabilities).as_deref(),
            Some("http://mac-book.local:8791")
        );
        assert_eq!(
            capabilities["agentdesk_api"]["session_forwarding"].as_bool(),
            Some(true)
        );

        let invalid = ClusterConfig {
            api_base_url: Some(" file:///tmp/agentdesk.sock ".to_string()),
            ..ClusterConfig::default()
        };
        let capabilities = cluster_capabilities_with_worker_api(&invalid);
        assert!(worker_api_base_url_from_capabilities(&capabilities).is_none());
    }

    #[test]
    fn env_api_base_url_fallback_is_published_when_config_is_empty() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        let _restore = EnvRestore::new();
        unsafe {
            std::env::set_var(
                "AGENTDESK_CLUSTER_API_BASE_URL",
                " https://worker.example.test:8791/ ",
            )
        };

        let capabilities = cluster_capabilities_with_worker_api(&ClusterConfig::default());
        assert_eq!(
            worker_api_base_url_from_capabilities(&capabilities).as_deref(),
            Some("https://worker.example.test:8791")
        );
    }

    #[test]
    fn enrich_session_owner_routing_handles_missing_owner_and_empty_nodes() {
        let mut sessions = vec![
            json!({"session_id": "local", "instance_id": "mac-mini-release"}),
            json!({"session_id": "missing", "instance_id": null}),
            json!({"session_id": "foreign", "instance_id": "mac-book-release"}),
        ];

        super::enrich_session_owner_routing(&mut sessions, Some("mac-mini-release"), &[]);

        assert_eq!(sessions[0]["owner"]["scope"].as_str(), Some("local"));
        assert_eq!(sessions[0]["owner"]["routable"].as_bool(), Some(false));
        assert_eq!(
            sessions[1]["owner"]["reason"].as_str(),
            Some("session_owner_missing")
        );
        assert_eq!(
            sessions[2]["owner"]["reason"].as_str(),
            Some("worker_node_missing")
        );
    }

    #[test]
    fn session_owner_summary_counts_stale_and_missing_owners() {
        let mut nodes = vec![
            json!({
                "instance_id": "mac-mini-release",
                "status": "online",
                "api_base_url": "http://mac-mini.local:8791"
            }),
            json!({
                "instance_id": "old-worker",
                "status": "offline",
                "api_base_url": "http://old-worker.local:8791"
            }),
        ];
        let mut sessions = vec![
            json!({"id": 1, "instance_id": "mac-mini-release"}),
            json!({"id": 2, "instance_id": "old-worker"}),
            json!({"id": 3, "instance_id": "missing-worker"}),
            json!({"id": 4, "instance_id": null}),
        ];

        super::enrich_session_owner_routing(&mut sessions, Some("mac-mini-release"), &nodes);
        attach_active_session_counts_to_worker_nodes(&mut nodes, &sessions);
        let summary = summarize_session_owner_routing(&sessions);

        assert_eq!(nodes[0]["active_session_count"].as_i64(), Some(1));
        assert_eq!(nodes[1]["active_session_count"].as_i64(), Some(1));
        assert_eq!(summary["total_active_sessions"].as_u64(), Some(4));
        assert_eq!(summary["local"].as_i64(), Some(1));
        assert_eq!(summary["foreign"].as_i64(), Some(2));
        assert_eq!(summary["unknown_owner"].as_i64(), Some(1));
        assert_eq!(summary["orphaned"].as_i64(), Some(2));
        assert_eq!(summary["unroutable"].as_i64(), Some(3));
        assert_eq!(summary["reasons"]["worker_node_stale"].as_i64(), Some(1));
        assert_eq!(summary["reasons"]["worker_node_missing"].as_i64(), Some(1));
        assert_eq!(
            summary["reasons"]["session_owner_missing"].as_i64(),
            Some(1)
        );
    }

    #[test]
    fn session_owner_routing_status_distinguishes_local_routable_and_stale() {
        let nodes = vec![
            json!({
                "instance_id": "mac-book-release",
                "status": "online",
                "api_base_url": "http://mac-book.local:8791"
            }),
            json!({
                "instance_id": "old-worker",
                "status": "offline",
                "api_base_url": "http://old-worker.local:8791"
            }),
            json!({
                "instance_id": "no-url-worker",
                "status": "online"
            }),
        ];

        let local = session_owner_routing_status(
            Some("mac-mini-release"),
            Some("mac-mini-release"),
            &nodes,
        );
        assert_eq!(local["scope"].as_str(), Some("local"));
        assert_eq!(local["is_local"].as_bool(), Some(true));

        let foreign = session_owner_routing_status(
            Some("mac-book-release"),
            Some("mac-mini-release"),
            &nodes,
        );
        assert_eq!(foreign["scope"].as_str(), Some("foreign"));
        assert_eq!(foreign["routable"].as_bool(), Some(true));
        assert_eq!(
            foreign["api_base_url"].as_str(),
            Some("http://mac-book.local:8791")
        );

        let stale =
            session_owner_routing_status(Some("old-worker"), Some("mac-mini-release"), &nodes);
        assert_eq!(stale["routable"].as_bool(), Some(false));
        assert_eq!(stale["reason"].as_str(), Some("worker_node_stale"));

        let missing_url =
            session_owner_routing_status(Some("no-url-worker"), Some("mac-mini-release"), &nodes);
        assert_eq!(missing_url["routable"].as_bool(), Some(false));
        assert_eq!(
            missing_url["reason"].as_str(),
            Some("worker_api_base_url_missing")
        );
    }
}
