//! A turn's strict resume requirement and the argv evidence of the execution it starts.
use serde::{Deserialize, Serialize};
use std::cell::RefCell;

#[derive(Clone)]
pub(crate) struct Requirement {
    pub provider: String,
    pub tmux: String,
    pub model: String,
    pub resume: Option<String>,
}
pub(crate) type Publisher = std::sync::Arc<dyn Fn(&Proof) -> Result<(), String> + Send + Sync>;
#[derive(Clone)]
struct State {
    requirement: Requirement,
    publisher: Option<Publisher>,
    published: bool,
}
thread_local! { static CURRENT: RefCell<Option<State>> = const { RefCell::new(None) }; }
struct Restore(Option<State>);
impl Drop for Restore {
    fn drop(&mut self) {
        CURRENT.with(|s| *s.borrow_mut() = self.0.take());
    }
}
#[cfg(test)]
pub(crate) fn scope<R>(requirement: Requirement, run: impl FnOnce() -> R) -> R {
    scope_with_publisher(requirement, None, run)
}
pub(crate) fn scope_with_publisher<R>(
    requirement: Requirement,
    publisher: Option<Publisher>,
    run: impl FnOnce() -> R,
) -> R {
    let state = State {
        requirement,
        publisher,
        published: false,
    };
    let _restore = Restore(CURRENT.with(|s| s.borrow_mut().replace(state)));
    run()
}
pub(crate) fn current() -> Option<Requirement> {
    CURRENT.with(|s| s.borrow().as_ref().map(|s| s.requirement.clone()))
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Proof {
    pub provider: String,
    pub tmux: String,
    pub nonce: String,
    pub session: String,
    pub model: String,
    pub argv_sha256: String,
}
pub(crate) fn path(tmux: &str) -> std::path::PathBuf {
    crate::services::tmux_common::session_temp_path(tmux, "routine_model.json").into()
}
pub(crate) fn matches(provider: &str, tmux: &str, nonce: &str, session: &str, model: &str) -> bool {
    std::fs::read(path(tmux))
        .ok()
        .and_then(|b| serde_json::from_slice::<Proof>(&b).ok())
        .is_some_and(|p| {
            p.provider == provider
                && p.tmux == tmux
                && p.nonce == nonce
                && p.session == session
                && p.model == model
                && !p.argv_sha256.is_empty()
        })
}
pub(crate) fn validate_selection(
    provider: &str,
    tmux: &str,
    session: &str,
    resume: bool,
) -> Result<(), String> {
    let Some(wanted) = current() else {
        return Ok(());
    };
    if wanted.provider != provider
        || wanted.tmux != tmux
        || wanted
            .resume
            .as_ref()
            .is_some_and(|s| s != session || !resume)
    {
        return Err("NotSent(\"routine_resume_selector_changed\")".into());
    }
    Ok(())
}
/// Written from the argv the launch script consumes; a spawn marker of this nonce admits reuse.
pub(crate) fn prepared(
    provider: &str,
    tmux: &str,
    session: &str,
    resume: bool,
    nonce: &str,
    args: &[String],
) -> Result<(), String> {
    let Some(wanted) = current() else {
        return Ok(());
    };
    validate_selection(provider, tmux, session, resume)?;
    let model = args
        .windows(2)
        .find(|p| matches!(p[0].as_str(), "--model" | "-m"))
        .map(|p| p[1].as_str());
    let resumed = match provider {
        "claude" => args
            .windows(2)
            .any(|p| p[0] == "--resume" && p[1] == session),
        "codex" => args.first().is_some_and(|a| a == "resume") && args.iter().any(|a| a == session),
        _ => false,
    };
    if model != Some(wanted.model.as_str()) || (wanted.resume.is_some() && !resumed) {
        return Err("NotSent(\"routine_model_argv_mismatch\")".into());
    }
    use sha2::{Digest, Sha256};
    let proof = Proof {
        provider: provider.into(),
        tmux: tmux.into(),
        nonce: nonce.into(),
        session: session.into(),
        model: wanted.model,
        argv_sha256: format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(args).map_err(|e| e.to_string())?)
        ),
    };
    save(&proof)
}
fn save(proof: &Proof) -> Result<(), String> {
    let path = path(&proof.tmux);
    let parent = path
        .parent()
        .ok_or("routine model proof has no directory")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let mut out = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    use std::io::Write;
    out.write_all(&serde_json::to_vec(proof).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    out.persist(path).map_err(|e| e.to_string())?;
    Ok(())
}
/// The existing owner validates the target epoch; this adds the same execution's model proof.
pub(crate) fn before_payload(target: &serde_json::Value) -> Result<(), String> {
    let Some(wanted) = current() else {
        return Ok(());
    };
    if target["host"] != "tmux" || target["owner"] != wanted.tmux {
        return Err("NotSent(\"routine_model_target_changed\")".into());
    }
    let nonce = target["execution_nonce"]
        .as_str()
        .ok_or("NotSent(\"routine_model_execution_unproved\")")?;
    let bytes = std::fs::read(path(&wanted.tmux))
        .map_err(|_| "NotSent(\"routine_model_launch_unproved\")")?;
    let proof: Proof =
        serde_json::from_slice(&bytes).map_err(|_| "NotSent(\"routine_model_launch_unproved\")")?;
    if !matches(
        &wanted.provider,
        &wanted.tmux,
        nonce,
        &proof.session,
        &wanted.model,
    ) || wanted.resume.as_ref().is_some_and(|s| s != &proof.session)
    {
        return Err("NotSent(\"routine_model_execution_changed\")".into());
    }
    Ok(())
}

/// A fresh Codex launch learns its native selector from its authenticated rollout binding.
pub(crate) fn bind_session(
    provider: &str,
    tmux: &str,
    nonce: &str,
    session: &str,
) -> Result<(), String> {
    let p = path(tmux);
    let bytes = std::fs::read(&p).map_err(|e| e.to_string())?;
    let mut proof: Proof = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if proof.provider != provider || proof.tmux != tmux || proof.nonce != nonce {
        return Err("foreign model execution".into());
    }
    if proof.session == session {
        return Ok(());
    }
    if provider != "codex" || !proof.session.is_empty() {
        return Err("foreign model session".into());
    }
    proof.session = session.to_string();
    save(&proof)
}
pub(crate) fn before_current_payload() -> Result<(), String> {
    let Some(wanted) = current() else {
        return Ok(());
    };
    use crate::services::tui_prompt_dedupe::binding_context::{
        SpawnNonceMarker, observe_spawn_nonce_marker,
    };
    let SpawnNonceMarker::Known(nonce) = observe_spawn_nonce_marker(&wanted.tmux) else {
        return Err("NotSent(\"routine_model_execution_unproved\")".into());
    };
    before_payload(
        &serde_json::json!({"host":"tmux","owner":wanted.tmux,"execution_nonce":nonce}),
    )?;
    let proof: Proof =
        serde_json::from_slice(&std::fs::read(path(&wanted.tmux)).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    publish(&proof)
}

fn publish(proof: &Proof) -> Result<(), String> {
    let publisher = CURRENT.with(|s| {
        s.borrow()
            .as_ref()
            .filter(|s| !s.published)
            .and_then(|s| s.publisher.clone())
    });
    if let Some(publisher) = publisher {
        publisher(proof)?;
        CURRENT.with(|s| {
            if let Some(s) = s.borrow_mut().as_mut() {
                s.published = true;
            }
        });
    }
    Ok(())
}
pub(crate) fn spawned(provider: &str, tmux: &str, nonce: &str) -> Result<(), String> {
    let Some(wanted) = current() else {
        return Ok(());
    };
    let bytes =
        std::fs::read(path(tmux)).map_err(|_| "NotSent(\"routine_model_spawn_unproved\")")?;
    let proof: Proof = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if wanted.provider != provider
        || wanted.tmux != tmux
        || proof.nonce != nonce
        || proof.model != wanted.model
    {
        return Err("NotSent(\"routine_model_spawn_changed\")".into());
    }
    publish(&proof)
}

#[cfg(test)]
#[path = "routine_model_tests.rs"]
mod tests;
