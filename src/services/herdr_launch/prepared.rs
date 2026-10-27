//! Physical create is independent of the subsequent pane/evidence projection.
use super::*;

pub(super) struct Created {
    pub witness: ServerWitness,
    pub created: Option<HerdrCreateOutcome>,
    pub sent_at: SystemTime,
}

pub(super) async fn create(
    pool: &PgPool,
    owner: &HostedOwner,
    nonce: &str,
    endpoint: &HerdrLaunchEndpoint,
    command: HerdrLaunchCommand,
    host: &Arc<dyn HerdrLaunchHost>,
    create_permit: Option<crate::services::session_host::policy::permit::CreatePermit>,
) -> Result<Created, HerdrLaunchError> {
    let (witness, created, sent_at) = if let Some(permit) = create_permit {
        let grant = permit.grant(nonce);
        if let Err(detail) = host.grants().save(grant.clone()).await {
            return Err(retire_unsent(
                pool,
                owner,
                nonce,
                HerdrLaunchError::GrantUnavailable(detail),
            )
            .await);
        }
        host.remember_grant(grant);
        let witness = permit.witness().clone();
        let request = UserDefaultCreateRequest {
            endpoint: endpoint.clone(),
            label: owner.logical_key.clone(),
            cwd: command.cwd,
            command: command.command,
            permit,
        };
        let sent_at = SystemTime::now();
        let created =
            on_blocking_thread(host, move |host| host.create_user_default(&request)).await;
        (witness, created, sent_at)
    } else {
        // Repeat E7 immediately before the legacy create, preserving its original ordering.
        let Some(restore_off_witness) = restore_off_witness(host, endpoint).await else {
            let error = HerdrLaunchError::NotSent(RESTORE_RESUME_NOT_OFF.into());
            return Err(retire_unsent(pool, owner, nonce, error).await);
        };
        let witness = restore_off_witness.clone();
        let request = HerdrCreateRequest {
            endpoint: endpoint.clone(),
            label: owner.logical_key.clone(),
            cwd: command.cwd,
            command: command.command,
            restore_off_witness,
        };
        let sent_at = SystemTime::now();
        let created = on_blocking_thread(host, move |host| host.create(&request)).await;
        (witness, created, sent_at)
    };
    Ok(Created {
        witness,
        created,
        sent_at,
    })
}
