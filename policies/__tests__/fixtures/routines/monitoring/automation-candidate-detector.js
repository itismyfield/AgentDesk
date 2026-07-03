(function () {
  const EMIT_RETRY_MS = 60 * 60 * 1000;
  const MAX_EVIDENCE_AGE_MS = 48 * 60 * 60 * 1000;

  function initCheckpoint(checkpoint) {
    return checkpoint && checkpoint.version === 1
      ? JSON.parse(JSON.stringify(checkpoint))
      : {
          version: 1,
          seen_candidates: {},
          stats: { ticks: 0, skipped_quality_gate: 0 },
        };
  }

  function suffixFromMarker(value, marker) {
    const prefix = `routine_observation:${marker}:`;
    if (typeof value === "string" && value.startsWith(prefix)) {
      return value.slice(prefix.length);
    }
    const kvPrefix = `kv_meta:${prefix}`;
    if (typeof value === "string" && value.startsWith(kvPrefix)) {
      return value.slice(kvPrefix.length);
    }
    return null;
  }

  function markerSignature(obs, marker) {
    if (marker === "candidate_review" && obs && obs.value && obs.value.signature) {
      return obs.value.signature;
    }
    return suffixFromMarker(obs && obs.key, marker) || suffixFromMarker(obs && obs.evidence_ref, marker);
  }

  function validReview(obs, now) {
    const value = (obs && obs.value) || {};
    if ((value.score || 0) < 80 || (value.evidence_count || 0) < 5) {
      return false;
    }
    const lastSeen = Date.parse(value.last_seen_at);
    if (!Number.isFinite(lastSeen)) {
      return false;
    }
    return now.getTime() - lastSeen <= MAX_EVIDENCE_AGE_MS;
  }

  function tick(ctx) {
    const now = new Date(ctx.now);
    const nowIso = now.toISOString();
    const observations = Array.isArray(ctx.observations) ? ctx.observations : [];
    const checkpoint = initCheckpoint(ctx.checkpoint);
    checkpoint.stats.ticks += 1;

    const approved = new Set();
    const dispatched = new Set();
    for (const obs of observations) {
      const approvedSig = markerSignature(obs, "candidate_approved");
      if (approvedSig) approved.add(approvedSig);
      const dispatchedSig = markerSignature(obs, "candidate_dispatched");
      if (dispatchedSig) dispatched.add(dispatchedSig);
    }

    for (const obs of observations) {
      const signature = markerSignature(obs, "candidate_review");
      if (!signature || approved.has(signature) || dispatched.has(signature)) {
        continue;
      }
      if (!validReview(obs, now)) {
        checkpoint.stats.skipped_quality_gate += 1;
        continue;
      }

      const seen = checkpoint.seen_candidates[signature];
      if (seen && seen.last_emitted_at) {
        const elapsed = now.getTime() - Date.parse(seen.last_emitted_at);
        if (Number.isFinite(elapsed) && elapsed < EMIT_RETRY_MS) {
          continue;
        }
      }

      checkpoint.seen_candidates[signature] = {
        status: "emitted",
        first_seen_at: seen && seen.first_seen_at ? seen.first_seen_at : nowIso,
        last_emitted_at: nowIso,
      };

      return {
        action: "agent",
        prompt: [
          `Approve automation candidate ${signature}.`,
          `Write routine_observation:candidate_approved:${signature} when approved.`,
        ].join("\n"),
        checkpoint,
        result: { review_count: 1 },
      };
    }

    return {
      action: "complete",
      checkpoint,
      result: { review_count: observations.filter((obs) => markerSignature(obs, "candidate_review")).length },
    };
  }

  agentdesk.routines.register({ id: "fixture-automation-candidate-detector", tick });
})();
