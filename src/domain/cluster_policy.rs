//! Closed cluster policy values and declaration-only validation.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentExecutionNode {
    pub default_node_id: Option<String>,
}

impl AgentExecutionNode {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.default_node_id.as_ref().is_some_and(|id| {
            id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        }) {
            return Err("default_node_id must be null or a valid node instance ID");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ExecutionRequirements {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub os: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub arch: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub backends: Vec<String>,
}

impl ExecutionRequirements {
    pub(crate) fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    pub(crate) fn parse(value: Value) -> Result<Self, String> {
        let policy: Self = serde_json::from_value(value)
            .map_err(|e| format!("invalid execution requirements: {e}"))?;
        for values in [
            &policy.os,
            &policy.arch,
            &policy.nodes,
            &policy.tools,
            &policy.repositories,
            &policy.backends,
        ] {
            if values.len() > 32
                || values.iter().any(|v| {
                    v.is_empty()
                        || v.len() > 128
                        || v.trim() != v
                        || v.chars().any(char::is_control)
                })
            {
                return Err(
                    "execution requirements contain too many, empty or invalid identifiers".into(),
                );
            }
        }
        if policy
            .os
            .iter()
            .any(|v| !["windows", "linux", "macos"].contains(&v.as_str()))
            || policy
                .arch
                .iter()
                .any(|v| !["x86_64", "aarch64"].contains(&v.as_str()))
            || policy
                .backends
                .iter()
                .any(|v| !["process", "tmux"].contains(&v.as_str()))
        {
            return Err("unsupported execution OS, architecture or backend".into());
        }
        if policy.nodes.iter().chain(&policy.tools).any(|v| {
            !v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        }) {
            return Err("node/tool requirements must be simple identifiers".into());
        }
        if policy.repositories.iter().any(|v| {
            let parts: Vec<_> = v.split('/').collect();
            parts.len() != 2
                || parts.iter().any(|p| {
                    p.is_empty()
                        || *p == "."
                        || *p == ".."
                        || !p
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                })
        }) {
            return Err("repository requirements must use logical owner/repository IDs".into());
        }
        Ok(policy)
    }
}

pub(crate) const ATTACHMENT_CAPABILITY: &str = "attachment_bundle_v1";

pub(crate) fn supports_attachment_transfer(node: &serde_json::Value) -> bool {
    node.pointer("/capabilities/intake_worker/features")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|features| {
            features
                .iter()
                .any(|feature| feature == ATTACHMENT_CAPABILITY)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strict_kernels_preserve_allowed_values_and_identifier_boundaries() {
        for os in ["windows", "linux", "macos"] {
            for arch in ["x86_64", "aarch64"] {
                for backend in ["process", "tmux"] {
                    let input = json!({"os":[os],"arch":[arch],"backends":[backend],
                        "nodes":["Worker_1.dev-x"],"tools":["git"],"repositories":["org-1/repo.name"]});
                    let parsed = ExecutionRequirements::parse(input.clone()).unwrap();
                    assert_eq!(serde_json::to_value(parsed).unwrap(), input);
                }
            }
        }
        let empty = ExecutionRequirements::parse(json!({})).unwrap();
        assert!(empty.is_empty());
        assert_eq!(serde_json::to_value(empty).unwrap(), json!({}));
        let max_id = "a".repeat(128);
        assert!(ExecutionRequirements::parse(json!({"nodes":vec![max_id.clone();32]})).is_ok());
        for input in [
            json!({"os":["Windows"]}),
            json!({"arch":["arm64"]}),
            json!({"backends":["ssh"]}),
            json!({"os":"linux"}),
            json!({"unknown":true}),
            json!({"nodes":vec!["node";33]}),
            json!({"tools":["a".repeat(129)]}),
            json!({"nodes":[""]}),
            json!({"nodes":[" node"]}),
            json!({"tools":["gi\nt"]}),
            json!({"nodes":["nœud"]}),
            json!({"tools":["bin/git"]}),
            json!({"repositories":["repo"]}),
            json!({"repositories":["owner//repo"]}),
            json!({"repositories":["../repo"]}),
            json!({"repositories":["owner/."]}),
        ] {
            assert!(
                ExecutionRequirements::parse(input.clone()).is_err(),
                "{input}"
            );
        }
        for default_node_id in [None, Some(max_id)] {
            assert!(AgentExecutionNode { default_node_id }.validate().is_ok());
        }
        for id in ["a".repeat(129), String::new(), "node/child".into()] {
            assert!(
                AgentExecutionNode {
                    default_node_id: Some(id)
                }
                .validate()
                .is_err()
            );
        }
        assert!(serde_json::from_value::<AgentExecutionNode>(json!({"extra":true})).is_err());
    }

    #[test]
    fn attachment_supports_reads_only_explicit_feature_declarations() {
        assert_eq!(ATTACHMENT_CAPABILITY, "attachment_bundle_v1");
        let mut node = json!({"status":"offline","forwarding":{"enabled":false},
            "capabilities":{"execution_readiness":{"expires_at_ms":0},
                "intake_worker":{"features":["other", "attachment_bundle_v1", 7]}}});
        assert!(supports_attachment_transfer(&node));
        node["status"] = json!("online");
        node["forwarding"]["enabled"] = json!(true);
        node["capabilities"]["execution_readiness"]["expires_at_ms"] = json!(i64::MAX);
        assert!(supports_attachment_transfer(&node));
        for features in [
            json!([]),
            json!(["Attachment_Bundle_V1"]),
            json!(["attachment_bundle_v10"]),
            json!("attachment_bundle_v1"),
            json!({"attachment_bundle_v1":true}),
            json!([true, null, 1]),
            json!(null),
        ] {
            node["capabilities"]["intake_worker"]["features"] = features;
            assert!(!supports_attachment_transfer(&node));
        }
        for misplaced in [
            json!({}),
            json!({"features":["attachment_bundle_v1"]}),
            json!({"capabilities":{"features":["attachment_bundle_v1"]}}),
            json!({"capabilities":{"intake_worker":["attachment_bundle_v1"]}}),
        ] {
            assert!(!supports_attachment_transfer(&misplaced));
        }
    }
}
