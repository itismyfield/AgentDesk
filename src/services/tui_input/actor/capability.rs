//! Attach-local Claude Busy permission; no snapshot is serialized or restored from the ledger.

use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding};

pub const CLAUDE_BUSY_VERSION: &str = "2.1.295";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FoldProfile {
    pub rows: u16,
    pub max_unfolded_newlines: u8,
    pub max_unfolded_codepoints: usize,
}

impl FoldProfile {
    pub fn measured(rows: u16) -> Self {
        Self {
            rows,
            max_unfolded_newlines: rows.saturating_sub(10).min(2) as u8,
            max_unfolded_codepoints: 800,
        }
    }
}

/// The attach owner supplies current launch, transcript and pane proofs, never config defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachEvidence {
    pub binding: SourceBinding,
    pub execution_nonce: String,
    pub launch_nonce: Option<String>,
    pub version: Option<String>,
    pub rows: u16,
    pub fold_profile: Option<FoldProfile>,
    pub actual_hooks: bool,
    pub source_verified: bool,
    pub pane_process_live: bool,
    pub gate_verified: bool,
    pub permission_mode: Option<String>,
}

#[derive(Debug, Default)]
pub struct CapabilitySnapshot {
    evidence: Option<AttachEvidence>,
    busy: bool,
}

impl CapabilitySnapshot {
    pub fn enabled(&self) -> bool {
        self.busy
    }
    pub fn derive(evidence: AttachEvidence) -> Self {
        Self {
            busy: verified(&evidence),
            evidence: Some(evidence),
        }
    }

    pub fn allows(&self, binding: &SourceBinding, nonce: &str) -> bool {
        #[cfg(test)]
        if super::super::transition::mutant("capability_nonce") {
            return self.busy;
        }
        self.busy
            && self.evidence.as_ref().is_some_and(|evidence| {
                &evidence.binding == binding && evidence.execution_nonce == nonce
            })
    }

    pub fn validate(&mut self, binding: &SourceBinding, nonce: Option<&str>) -> bool {
        let allowed = nonce.is_some_and(|nonce| self.allows(binding, nonce));
        self.busy &= allowed;
        self.busy
    }

    /// Evidence drift is permanent for this attach; only a new incarnation may derive again.
    pub fn revalidate(&mut self, evidence: &AttachEvidence) -> bool {
        #[cfg(test)]
        if super::super::transition::mutant("capability_upgrade") {
            self.busy = true;
        }
        self.busy &= self.evidence.as_ref() == Some(evidence) && verified(evidence);
        self.busy
    }

    pub fn invalidate(&mut self) {
        self.busy = false;
    }
}

fn verified(evidence: &AttachEvidence) -> bool {
    let version = evidence.version.as_deref() == Some(CLAUDE_BUSY_VERSION);
    let profile =
        evidence.rows > 0 && evidence.fold_profile == Some(FoldProfile::measured(evidence.rows));
    let permission = matches!(
        evidence.permission_mode.as_deref(),
        Some("auto" | "default" | "acceptEdits" | "plan" | "dontAsk" | "bypassPermissions")
    );
    #[cfg(test)]
    let version = version || super::super::transition::mutant("capability_version");
    #[cfg(test)]
    let profile = profile || super::super::transition::mutant("capability_profile");
    #[cfg(test)]
    let permission = permission || super::super::transition::mutant("capability_permission");
    evidence.binding.provider == ShadowProvider::Claude
        && !evidence.execution_nonce.is_empty()
        && evidence.launch_nonce.as_deref() == Some(evidence.execution_nonce.as_str())
        && version
        && profile
        && evidence.actual_hooks
        && evidence.source_verified
        && evidence.pane_process_live
        && evidence.gate_verified
        && permission
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tui_o::shadow::SourceId;

    fn evidence() -> AttachEvidence {
        AttachEvidence {
            binding: SourceBinding {
                channel_id: 42,
                provider: ShadowProvider::Claude,
                source: SourceId {
                    session_id: "native-session".into(),
                    path: "/test/transcript.jsonl".into(),
                    dev: 1,
                    ino: 2,
                },
            },
            execution_nonce: "incarnation-0".into(),
            launch_nonce: Some("incarnation-0".into()),
            version: Some(CLAUDE_BUSY_VERSION.into()),
            rows: 24,
            fold_profile: Some(FoldProfile::measured(24)),
            actual_hooks: true,
            source_verified: true,
            pane_process_live: true,
            gate_verified: true,
            permission_mode: Some("auto".into()),
        }
    }

    #[test]
    fn capability_default_and_new_restart_snapshot_are_off() {
        let proof = evidence();
        let mut old = CapabilitySnapshot::derive(proof.clone());
        assert!(old.validate(&proof.binding, Some(&proof.execution_nonce)));
        assert!(!CapabilitySnapshot::default().allows(&proof.binding, &proof.execution_nonce));
        old.invalidate();
        assert!(!old.validate(&proof.binding, Some(&proof.execution_nonce)));
        assert!(!old.revalidate(&proof));
    }

    #[test]
    fn capability_gate_rejects_each_missing_or_unknown_proof() {
        let proof = evidence();
        let mut variants = Vec::new();
        let mut add = |label, change: fn(&mut AttachEvidence)| {
            let mut missing = proof.clone();
            change(&mut missing);
            variants.push((label, missing));
        };
        add("provider", |e| e.binding.provider = ShadowProvider::Codex);
        add("missing launch", |e| e.launch_nonce = None);
        add("launch drift", |e| e.launch_nonce = Some("other".into()));
        add("empty nonce", |e| e.execution_nonce.clear());
        add("missing version", |e| e.version = None);
        add("version drift", |e| e.version = Some("2.1.292".into()));
        add("missing profile", |e| e.fold_profile = None);
        add("zero rows", |e| {
            e.rows = 0;
            e.fold_profile = Some(FoldProfile::measured(0));
        });
        add("profile rows", |e| {
            e.fold_profile.as_mut().unwrap().rows = 12
        });
        add("profile newline", |e| {
            e.fold_profile.as_mut().unwrap().max_unfolded_newlines = 3;
        });
        add("profile length", |e| {
            e.fold_profile.as_mut().unwrap().max_unfolded_codepoints = 801;
        });
        add("hooks", |e| e.actual_hooks = false);
        add("source", |e| e.source_verified = false);
        add("pane", |e| e.pane_process_live = false);
        add("gate", |e| e.gate_verified = false);
        add("missing permission", |e| e.permission_mode = None);
        add("empty permission", |e| {
            e.permission_mode = Some(String::new())
        });
        add("unknown permission", |e| {
            e.permission_mode = Some("unknown".into())
        });
        for (label, missing) in variants {
            let snapshot = CapabilitySnapshot::derive(missing);
            assert!(
                !snapshot.allows(&proof.binding, &proof.execution_nonce),
                "{label}"
            );
        }
    }

    #[test]
    fn capability_stale_snapshot_nonce_drift_permanently_disables() {
        let proof = evidence();
        for nonce in [None, Some("incarnation-1")] {
            let mut snapshot = CapabilitySnapshot::derive(proof.clone());
            assert!(!snapshot.validate(&proof.binding, nonce));
            assert!(!snapshot.validate(&proof.binding, Some(&proof.execution_nonce)));
            assert!(!snapshot.revalidate(&proof));
        }
    }

    #[test]
    fn capability_source_identity_drift_permanently_disables() {
        let proof = evidence();
        let mut replaced = proof.binding.clone();
        replaced.source.ino += 1;
        let mut snapshot = CapabilitySnapshot::derive(proof.clone());
        assert!(!snapshot.validate(&replaced, Some(&proof.execution_nonce)));
        assert!(!snapshot.validate(&proof.binding, Some(&proof.execution_nonce)));
    }

    #[test]
    fn capability_no_mid_incarnation_upgrade() {
        let proof = evidence();
        let mut snapshot = CapabilitySnapshot::derive(proof.clone());
        let mut drift = proof.clone();
        drift.actual_hooks = false;
        assert!(!snapshot.revalidate(&drift));
        assert!(!snapshot.revalidate(&proof));
        assert!(!snapshot.allows(&proof.binding, &proof.execution_nonce));
        let mut absent = CapabilitySnapshot::default();
        assert!(!absent.revalidate(&proof));
        let mut missing = proof.clone();
        missing.permission_mode = None;
        assert!(!CapabilitySnapshot::derive(missing).revalidate(&proof));
    }

    #[test]
    fn capability_profile_matches_measured_rows_boundaries() {
        for (rows, limit) in [(8, 0), (10, 0), (11, 1), (12, 2), (24, 2)] {
            let mut proof = evidence();
            proof.rows = rows;
            proof.fold_profile = Some(FoldProfile::measured(rows));
            assert_eq!(proof.fold_profile.unwrap().max_unfolded_newlines, limit);
            let mut snapshot = CapabilitySnapshot::derive(proof.clone());
            assert!(snapshot.validate(&proof.binding, Some(&proof.execution_nonce)));
            assert!(snapshot.revalidate(&proof));
            let mut resized = proof.clone();
            resized.rows += 1;
            resized.fold_profile = Some(FoldProfile::measured(resized.rows));
            assert!(!snapshot.revalidate(&resized));
        }
    }
}
