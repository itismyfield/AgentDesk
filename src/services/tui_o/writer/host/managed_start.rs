//! A managed writer starts only after its adoption and readiness claim agree.
use super::*;

/// [`start`] over the supplied owned channels.
pub fn start_managed<I: HostIo>(
    provider: ShadowProvider,
    pg_gateway: bool,
    owned: OwnedChannels,
    prepare: impl FnOnce() -> HostParts<I>,
) -> Vec<ManagedWriterHandle> {
    let kind = match provider {
        ShadowProvider::Claude => RuntimeHandoffKind::ClaudeTui,
        ShadowProvider::Codex => RuntimeHandoffKind::CodexTui,
    };
    let ours = |(channel, channel_kind, candidate): (u64, _, Option<Candidate>)| {
        Some((
            channel,
            candidate
                .filter(|candidate| channel_kind == Some(kind) && !candidate.child_parked())?,
        ))
    };
    let channels: Vec<_> = owned.into_iter().filter_map(ours).collect();
    if channels.is_empty() {
        return Vec::new();
    }
    let HostParts {
        io,
        runtime_root,
        gate,
        readiness,
    } = prepare();
    let mut tasks = Vec::new();
    for (channel, candidate) in channels {
        let Some(claim) = claim(&candidate, &readiness, channel) else {
            continue;
        };
        let keep = Arc::new(AtomicBool::new(false));
        let reservation = candidate.late_cleanup_guard(channel, runtime_root.clone());
        let reservation = reservation.map(|guard| guard.kept_by(Arc::clone(&keep)));
        let execution = io.execution_permit(channel);
        let authorized = match &execution {
            Some(Ok(_)) => true,
            Some(Err(_)) => false,
            None => pg_gateway,
        };
        let root = match (authorized, &runtime_root) {
            (false, _) => Err("no PG gateway lease"),
            (true, None) => Err("runtime root unresolved"),
            (true, Some(root)) => Ok(root.clone()),
        };
        let root = match root {
            Ok(root) => root,
            Err(detail) => {
                hold(&io.alarms(), channel, detail);
                drop(reservation);
                claim.release();
                continue;
            }
        };
        let gate = match execution {
            Some(Ok(permit)) => permit.gate(),
            _ => Arc::clone(&gate),
        };
        let generation = claim.generation;
        let settled = Arc::clone(&claim.settled);
        let (stop, stopping) = watch::channel(false);
        let retired_stop = stop.clone();
        let io = Arc::clone(&io);
        let host = tokio::spawn(async move {
            let retirement = Arc::clone(&gate);
            {
                let run = host_channel(
                    io,
                    channel,
                    candidate,
                    provider,
                    root,
                    gate,
                    (&claim, stopping),
                );
                tokio::pin!(run);
                tokio::select! {
                    () = &mut run => {},
                    () = retirement.execution_retired() => {
                        retired_stop.send_replace(true);
                        run.await;
                    }
                }
            }
            drop(reservation);
            if claim.settled.load(Ordering::SeqCst) {
                claim.release();
            }
        });
        tasks.push(ManagedWriterHandle {
            channel,
            generation,
            settled,
            stop: StopOnDrop {
                signal: stop,
                armed: true,
            },
            host,
            #[cfg(test)]
            keep,
        });
    }
    tasks
}

fn claim(
    candidate: &Candidate,
    readiness: &Arc<Readiness>,
    channel: u64,
) -> Option<ReadinessClaim> {
    let state = candidate.lock();
    if candidate.child_parked_in(*state) {
        return None;
    }
    readiness.claim(channel)
}
