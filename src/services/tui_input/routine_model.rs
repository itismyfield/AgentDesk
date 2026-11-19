//! A turn's strict resume requirement and the argv evidence of the execution it starts.
use serde::{Deserialize, Serialize};
use std::cell::RefCell;

#[derive(Clone)]
pub(crate) struct Requirement {
    pub provider: String,
    pub tmux: String,
    pub model: Option<String>,
    pub default_model: Option<String>,
    pub resume: Option<String>,
}

impl Requirement {
    pub(crate) fn launch_model(&self) -> Option<&str> {
        self.model.as_deref().or(self.default_model.as_deref())
    }

    pub(crate) fn matches(&self, nonce: &str, session: &str) -> bool {
        read(&self.tmux).is_some_and(|proof| self.verifies(&proof, nonce, session))
    }

    fn verifies(&self, proof: &Proof, nonce: &str, session: &str) -> bool {
        proof.provider == self.provider
            && proof.tmux == self.tmux
            && proof.nonce == nonce
            && proof.session == session
            && proof.model == self.model
            && proof.default_model == self.default_model
            && self
                .resume
                .as_deref()
                .is_none_or(|expected| expected == session)
            && !proof.argv_sha256.is_empty()
    }
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
/// The owner's pump executes its leaf on another thread; carry this turn's model requirement too.
pub(crate) fn carry<T, R>(run: impl FnOnce(T) -> R + Send) -> impl FnOnce(T) -> R + Send {
    let state = CURRENT.with(|s| s.borrow().clone());
    move |input| {
        let _restore = Restore(CURRENT.with(|s| std::mem::replace(&mut *s.borrow_mut(), state)));
        run(input)
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
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_model: Option<String>,
    pub argv_sha256: String,
}
pub(crate) fn path(tmux: &str) -> std::path::PathBuf {
    use crate::services::tmux_common::session_evidence::MODEL_PROOF_EXT;
    crate::services::tmux_common::session_temp_path(tmux, MODEL_PROOF_EXT).into()
}
fn read(tmux: &str) -> Option<Proof> {
    serde_json::from_slice(&std::fs::read(path(tmux)).ok()?).ok()
}

/// Retire S and report whether another execution still owns the proof file.
#[cfg(unix)]
pub(crate) fn retire(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    provider: &str,
    session: &str,
) -> Result<bool, String> {
    let tmux = authority.session();
    if let Some(proof) = read(tmux) {
        if proof.provider != provider || proof.tmux != tmux || proof.session != session {
            return Ok(true);
        }
        crate::services::tmux_common::session_evidence::remove(&path(tmux))?;
    }
    Ok(false)
}

/// A null claim needs the strict boundary when this pane already applied a routine model.
#[cfg(unix)]
pub(crate) fn tracks_live_execution(provider: &str, tmux: &str) -> bool {
    use crate::services::platform::tmux::{PaneLiveness, pane_liveness};
    use crate::services::tui_prompt_dedupe::binding_context::{
        SpawnNonceMarker, observe_spawn_nonce_marker,
    };

    let tracked = match std::fs::read(path(tmux)) {
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
        Ok(bytes) => serde_json::from_slice::<Proof>(&bytes).map_or(true, |proof| {
            proof.provider == provider
                && proof.tmux == tmux
                && !matches!(
                    observe_spawn_nonce_marker(tmux),
                    SpawnNonceMarker::Known(ref nonce) if nonce != &proof.nonce
                )
        }),
    };
    tracked && pane_liveness(tmux) != PaneLiveness::DeadOrAbsent
}

/// The native model can survive a pane's exit with its conversation.
#[cfg(unix)]
pub(crate) fn tracks_native_session(provider: &str, tmux: &str, session: &str) -> bool {
    read(tmux).is_some_and(|proof| {
        proof.provider == provider && proof.tmux == tmux && proof.session == session
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
    let option_end = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let model = args[..option_end]
        .windows(2)
        .find(|p| matches!(p[0].as_str(), "--model" | "-m"))
        .map(|p| p[1].as_str());
    let resumed = match provider {
        "claude" => args
            .windows(2)
            .any(|p| p[0] == "--resume" && p[1] == session),
        "codex" => codex_resumed(args, session),
        _ => false,
    };
    if model != wanted.launch_model() || (wanted.resume.is_some() && !resumed) {
        return Err("NotSent(\"routine_model_argv_mismatch\")".into());
    }
    use sha2::{Digest, Sha256};
    let proof = Proof {
        provider: provider.into(),
        tmux: tmux.into(),
        nonce: nonce.into(),
        session: session.into(),
        model: wanted.model,
        default_model: wanted.default_model,
        argv_sha256: format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(args).map_err(|e| e.to_string())?)
        ),
    };
    crate::services::tmux_common::with_tmux_source_authority(tmux, |_| save(&proof))
}
fn codex_resumed(args: &[String], session: &str) -> bool {
    let Some(end) = args.iter().position(|a| a == "--") else {
        return false;
    };
    let mut options = args[..end].iter().map(String::as_str);
    let first = options.next();
    let command = match first {
        Some("--dangerously-bypass-hook-trust") => options.next(),
        other => other,
    };
    command == Some("resume") && options.next_back() == Some(session)
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
/// An ordinary fallback changes the model without changing the execution nonce.
pub(crate) fn invalidate(tmux: &str) -> Result<(), String> {
    crate::services::tmux_common::with_tmux_source_authority(tmux, |_| {
        invalidate_under_authority(tmux)
    })
}

fn invalidate_under_authority(tmux: &str) -> Result<(), String> {
    let bytes = match std::fs::read(path(tmux)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let Ok(mut proof) = serde_json::from_slice::<Proof>(&bytes) else {
        return Ok(());
    };
    if proof.tmux == tmux && !proof.argv_sha256.is_empty() {
        proof.argv_sha256.clear();
        save(&proof)?;
    }
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
    if !wanted.verifies(&proof, nonce, &proof.session) {
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
    crate::services::tmux_common::with_tmux_source_authority(tmux, |authority| {
        bind_session_under_authority(authority, provider, nonce, session)
    })
}

pub(crate) fn bind_session_under_authority(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    provider: &str,
    nonce: &str,
    session: &str,
) -> Result<(), String> {
    let tmux = authority.session();
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
        || !wanted.verifies(&proof, nonce, &proof.session)
    {
        return Err("NotSent(\"routine_model_spawn_changed\")".into());
    }
    publish(&proof)
}

#[cfg(test)]
#[path = "routine_model_tests.rs"]
mod tests;
