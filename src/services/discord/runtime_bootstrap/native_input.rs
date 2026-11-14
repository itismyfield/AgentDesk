//! Native boot custody settles before this runtime opens any intake surface.
use super::*;

pub(in crate::services::discord::runtime_bootstrap) async fn prepare_native_boot(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) -> bool {
    #[cfg(unix)]
    if *provider == ProviderKind::Claude
        && super::super::super::turn_finalizer::native_prepare::enabled()
    {
        use super::super::super::turn_finalizer::native_prepare::BootInputs;
        let prepared = crate::services::discord::runtime_store::runtime_root()
            .ok_or_else(|| "native boot root unavailable".to_string())
            .and_then(|root| BootInputs::prepare(&root, shared.restart.current_generation));
        let plan = match prepared {
            Ok(plan) => plan,
            Err(error) => {
                tracing::warn!(%error, "native boot preparation refused; rows and custody retained");
                return false;
            }
        };
        let reaped = super::super::super::inflight::reap_inflight_rows_at_boot_blocking(
            provider,
            shared.pg_pool.clone(),
        )
        .await;
        if !reaped.native_preparation_complete() {
            tracing::warn!("native boot reaper preparation incomplete; restore kept pending");
            return false;
        }
        if let Err(error) = plan.restore(shared).await {
            tracing::warn!(%error, "native boot restore held before intake");
            return false;
        }
        return true;
    }
    super::super::super::inflight::reap_inflight_rows_at_boot_blocking(
        provider,
        shared.pg_pool.clone(),
    )
    .await;
    true
}
