//! A resumed input pins the source boundary before submit; only its new native turn may adopt.
use crate::services::{
    provider::{CancelToken, cancel_requested},
    tmux_common as tc,
    tui_o::{
        channel_policy::{
            Candidate, first_prompt_digest,
            output_disposition::{Identity, Output, Phase, Reason},
        },
        shadow::{
            MAX_READ_BYTES, ShadowProvider, SourceId,
            identity::{RecordFact, classify},
        },
    },
    tui_prompt_dedupe::{
        self as dedupe,
        binding_context::{self as bc, BindingContext},
    },
};
use std::{
    io::{Read, Seek, SeekFrom},
    sync::Arc,
};

pub(crate) struct ContinuationInput {
    context: BindingContext,
    source: SourceId,
    input: u64,
    token: Arc<CancelToken>,
    prompt_digest: String,
    start: u64,
}
pub(crate) struct ContinuationProof {
    input: ContinuationInput,
    native: String,
    preserved: Vec<u8>,
    identity: Identity,
}
fn open(source: &SourceId) -> Result<std::fs::File, String> {
    let file = std::fs::File::open(&source.path).map_err(|e| e.to_string())?;
    if crate::services::tui_o::shadow::capture::file_identity(
        &file.metadata().map_err(|e| e.to_string())?,
    ) != (source.dev, source.ino)
    {
        return Err("continuation source file changed".into());
    }
    Ok(file)
}
fn current<T>(
    context: &BindingContext,
    source: &SourceId,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    tc::with_tmux_source_authority(&context.tmux_session, |authority| {
        if context.owner_runtime_root != tc::current_tmux_owner_marker()
            || bc::execution_context("codex", &context.execution_nonce)? != *context
            || bc::observe_spawn_nonce_marker(authority.session())
                != bc::SpawnNonceMarker::Known(context.execution_nonce.clone())
            || dedupe::owner_channel_for_tmux_session(authority.session()) != context.channel_id
        {
            return Err("continuation execution changed".into());
        }
        let binding = dedupe::runtime_binding_for_tmux_session_under_source_authority(authority)
            .ok_or("continuation binding is pending")?;
        let fold =
            dedupe::binding_events::codex::read_ownership(context).map_err(|e| e.to_string())?;
        if fold.conflicted
            || fold.verified.as_ref().is_none_or(|p| p.source != *source)
            || binding.runtime_kind != crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
            || binding.session_id.as_deref() != Some(source.session_id.as_str())
            || !dedupe::codex_verified_source_allowed_under_source_authority(
                authority,
                &binding.output_path,
                binding.session_id.as_deref(),
            )
            || std::path::Path::new(&binding.output_path)
                .canonicalize()
                .ok()
                != source.path.canonicalize().ok()
            || context
                .provider_root
                .as_ref()
                .is_none_or(|root| !source.path.starts_with(root))
        {
            return Err("continuation canonical source changed".into());
        }
        action()
    })
}
impl ContinuationInput {
    pub(crate) fn before_submit(
        context: BindingContext,
        source: SourceId,
        input: u64,
        token: Arc<CancelToken>,
        prompt: &str,
    ) -> Result<Self, String> {
        if context.provider != "codex"
            || context.launch_mode != "resume"
            || context.expected_native_session_id.is_none()
            || context.channel_id.is_none()
            || token.turn_nonce().is_none()
            || cancel_requested(Some(&token))
        {
            return Err("continuation needs its original resumed execution and input".into());
        }
        let start = current(&context, &source, || {
            let mut file = open(&source)?;
            let end = file.metadata().map_err(|e| e.to_string())?.len();
            if end > 0 {
                file.seek(SeekFrom::Start(end - 1))
                    .map_err(|e| e.to_string())?;
                let mut last = [0];
                file.read_exact(&mut last).map_err(|e| e.to_string())?;
                if last != [b'\n'] {
                    return Err("continuation input boundary is incomplete".into());
                }
            }
            Ok(end)
        })?;
        Ok(Self {
            context,
            source,
            input,
            token,
            prompt_digest: first_prompt_digest(prompt),
            start,
        })
    }
    fn window(&self) -> Result<Vec<u8>, String> {
        let mut file = open(&self.source)?;
        file.seek(SeekFrom::Start(self.start))
            .map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        file.take(MAX_READ_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_READ_BYTES || bytes.last() != Some(&b'\n') {
            return Err("continuation window is incomplete or oversized".into());
        }
        Ok(bytes)
    }
}
impl ContinuationProof {
    pub(crate) fn capture(input: ContinuationInput) -> Result<Self, String> {
        let preserved = input.window()?;
        let native = check_turn(&preserved, &input.prompt_digest)?;
        let identity = Identity {
            channel: input
                .context
                .channel_id
                .ok_or("continuation channel missing")?,
            input: input.input,
            nonce: first_prompt_digest(
                input
                    .token
                    .turn_nonce()
                    .ok_or("continuation input nonce missing")?,
            ),
            execution: first_prompt_digest(&input.context.execution_nonce),
            source: first_prompt_digest(
                &serde_json::to_string(&(
                    &input.source,
                    input.start,
                    &native,
                    &input.prompt_digest,
                ))
                .map_err(|e| e.to_string())?,
            ),
        };
        Ok(Self {
            input,
            native,
            preserved,
            identity,
        })
    }
    pub(crate) fn identity(&self) -> Identity {
        self.identity.clone()
    }
    pub(crate) fn with_current<T>(
        &self,
        action: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        if cancel_requested(Some(&self.input.token)) {
            return Err("continuation input cancelled".into());
        }
        current(&self.input.context, &self.input.source, || {
            let store = crate::services::tui_o::store::OStore::existing(std::path::Path::new(
                &self.input.context.owner_runtime_root,
            ))
            .ok_or("continuation has no O store")?;
            let init = store
                .read_init(self.identity.channel)
                .map_err(|e| e.to_string())?
                .ok_or("continuation has no O init")?;
            if !init
                .sources
                .iter()
                .any(|s| s.source_id == self.input.source)
            {
                return Err("continuation O source is not initialized".into());
            }
            let bytes = self.input.window()?;
            if !bytes.starts_with(&self.preserved)
                || check_turn(&bytes, &self.input.prompt_digest)? != self.native
            {
                return Err("continuation prefix changed".into());
            }
            action()
        })
    }
}
fn check_turn(bytes: &[u8], digest: &str) -> Result<String, String> {
    let (mut native, mut prompts, mut closed) = (None, std::collections::HashSet::new(), false);
    for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let value: serde_json::Value = serde_json::from_slice(line).map_err(|e| e.to_string())?;
        if value["payload"]["turn_id"]
            .as_str()
            .is_some_and(|id| native.as_deref().is_some_and(|native| native != id))
        {
            return Err("continuation native turn changed".into());
        }
        for fact in classify(ShadowProvider::Codex, &value) {
            if closed {
                return Err("continuation record follows its turn end".into());
            }
            match fact {
                RecordFact::TurnStart(Some(id)) if native.is_none() => native = Some(id),
                RecordFact::TurnStart(_) | RecordFact::Blocked(_) => {
                    return Err("continuation turn is unproved".into());
                }
                RecordFact::Prompt(..) if dedupe::is_codex_context_record(&value["payload"]) => {}
                RecordFact::Prompt(_, text)
                    if native.is_some()
                        && first_prompt_digest(&text) == digest
                        && prompts.insert(value["type"].as_str().unwrap_or("").to_owned()) => {}
                RecordFact::Prompt(..) => return Err("continuation input changed".into()),
                RecordFact::Idle(_) => closed = true,
                _ if native.is_none() => {
                    return Err("continuation has no original turn opener".into());
                }
                _ => {}
            }
        }
    }
    native
        .filter(|_| !prompts.is_empty())
        .ok_or("continuation has no original prompt".into())
}
pub(crate) async fn run(
    output: &Output,
    proof: &ContinuationProof,
    candidate: &Candidate,
) -> Result<(), Reason> {
    loop {
        if output.expire() || output.remaining().is_zero() {
            return Err(Reason::Deadline);
        }
        if output.phase() != Phase::Normalizing {
            return Err(Reason::Invalidated);
        }
        match output.adopt_resumed(proof, candidate) {
            Ok(()) => return Ok(()),
            Err(error) if error == "continuation binding is pending" => {
                tokio::time::sleep(
                    output
                        .remaining()
                        .min(tokio::time::Duration::from_millis(100)),
                )
                .await
            }
            Err(_) => {
                if output.expire() || output.remaining().is_zero() {
                    return Err(Reason::Deadline);
                }
                output.reject(Reason::SourceRejected);
                return Err(Reason::SourceRejected);
            }
        }
    }
}
#[path = "continuation_tests.rs"]
mod tests;
