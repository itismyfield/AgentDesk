//! Test-only synchronization barriers for the manual-rebind race seams
//! (#4712 de-giant split from `manual_rebind/mod.rs`).
//!
//! Pure move — zero logic change: the `PostAdoptionClaimBarrier` /
//! `EpisodeAuthorityHeldBarrier` pair, their process-global install slots,
//! restore-on-drop guards, and the `await_*` seam hooks that
//! `rebind_inflight_for_channel_inner` / `episode_handoff` call under
//! `#[cfg(test)]`. Every item keeps its own `#[cfg(test)]` gate exactly as it
//! had in the parent module, so this file stays a plain (ungated) submodule and
//! introduces no new logical test module. The two `await_*` hooks were private
//! to the parent module and are elevated to `pub(super)` — the minimum surface
//! for the parent and its `episode_handoff` sibling to keep calling them; the
//! barrier types and `install_*` fns keep their original `pub(crate)`
//! visibility (re-exported by the parent for the `recovery_engine` root and
//! `post_adoption_guard_tests`).

#[cfg(test)]
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct PostAdoptionClaimBarrier {
    pub(crate) reached: std::sync::Arc<tokio::sync::Barrier>,
    pub(crate) resume: std::sync::Arc<tokio::sync::Barrier>,
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct EpisodeAuthorityHeldBarrier {
    pub(crate) reached: std::sync::Arc<tokio::sync::Barrier>,
    pub(crate) resume: std::sync::Arc<tokio::sync::Barrier>,
}

#[cfg(test)]
fn post_adoption_claim_barrier_slot() -> &'static Mutex<Option<PostAdoptionClaimBarrier>> {
    static SLOT: OnceLock<Mutex<Option<PostAdoptionClaimBarrier>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn episode_authority_held_barrier_slot() -> &'static Mutex<Option<EpisodeAuthorityHeldBarrier>> {
    static SLOT: OnceLock<Mutex<Option<EpisodeAuthorityHeldBarrier>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
pub(crate) struct PostAdoptionClaimBarrierGuard(Option<PostAdoptionClaimBarrier>);

#[cfg(test)]
pub(crate) struct EpisodeAuthorityHeldBarrierGuard(Option<EpisodeAuthorityHeldBarrier>);

#[cfg(test)]
impl Drop for PostAdoptionClaimBarrierGuard {
    fn drop(&mut self) {
        *post_adoption_claim_barrier_slot()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = self.0.take();
    }
}

#[cfg(test)]
impl Drop for EpisodeAuthorityHeldBarrierGuard {
    fn drop(&mut self) {
        *episode_authority_held_barrier_slot()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = self.0.take();
    }
}

#[cfg(test)]
pub(crate) fn install_post_adoption_claim_barrier(
    barrier: PostAdoptionClaimBarrier,
) -> PostAdoptionClaimBarrierGuard {
    let previous = post_adoption_claim_barrier_slot()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .replace(barrier);
    PostAdoptionClaimBarrierGuard(previous)
}

#[cfg(test)]
pub(crate) fn install_episode_authority_held_barrier(
    barrier: EpisodeAuthorityHeldBarrier,
) -> EpisodeAuthorityHeldBarrierGuard {
    let previous = episode_authority_held_barrier_slot()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .replace(barrier);
    EpisodeAuthorityHeldBarrierGuard(previous)
}

#[cfg(test)]
pub(super) async fn await_post_adoption_claim_barrier() {
    let barrier = post_adoption_claim_barrier_slot()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    if let Some(barrier) = barrier {
        barrier.reached.wait().await;
        barrier.resume.wait().await;
    }
}

#[cfg(test)]
pub(super) async fn await_episode_authority_held_barrier() {
    let barrier = episode_authority_held_barrier_slot()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    if let Some(barrier) = barrier {
        barrier.reached.wait().await;
        barrier.resume.wait().await;
    }
}
