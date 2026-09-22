//! One hard capability contract shared by dispatch and human intake.
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

    pub(crate) fn explain(&self, node: &Value, now: i64) -> Vec<String> {
        if self.is_empty() {
            return Vec::new();
        }
        let mut reasons = Vec::new();
        if node["status"] != "online" {
            reasons.push("node_offline".into());
        }
        if !self.nodes.is_empty() && !self.nodes.iter().any(|id| node["instance_id"] == *id) {
            reasons.push("required_node_mismatch".into());
        }
        let probe = match super::readiness::evidence(node, now) {
            Ok(probe) => probe,
            Err(reason) => {
                reasons.push(reason.into());
                return reasons;
            }
        };
        if !self.os.is_empty() && !self.os.contains(&probe.os) {
            reasons.push("required_os_mismatch".into());
        }
        if !self.arch.is_empty() && !self.arch.contains(&probe.arch) {
            reasons.push("required_arch_mismatch".into());
        }
        for name in &self.tools {
            if probe.tools.get(name) != Some(&true) {
                reasons.push(format!("required_tool_unavailable:{name}"));
            }
        }
        for id in &self.repositories {
            if probe.repositories.get(id) != Some(&true) {
                reasons.push(format!("required_repository_unavailable:{id}"));
            }
        }
        for backend in &self.backends {
            if !probe.backends.contains(backend) {
                reasons.push(format!("required_backend_unavailable:{backend}"));
            }
        }
        reasons
    }

    pub(crate) fn intake_reasons(&self, node: &Value) -> Vec<String> {
        if self.is_empty() {
            return Vec::new();
        }
        let mut reasons = self.explain(node, chrono::Utc::now().timestamp_millis());
        if !node
            .pointer("/capabilities/intake_worker/features")
            .and_then(Value::as_array)
            .is_some_and(|features| {
                features
                    .iter()
                    .any(|feature| feature == "execution_requirements_v1")
            })
        {
            reasons.push("execution_requirements_protocol_missing".into());
        }
        reasons
    }
}

#[cfg(test)]
pub(super) mod tests;
