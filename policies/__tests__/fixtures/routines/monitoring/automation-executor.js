(function () {
  function initCheckpoint(checkpoint) {
    return checkpoint && checkpoint.version === 1
      ? JSON.parse(JSON.stringify(checkpoint))
      : {
          version: 1,
          dispatched_signatures: {},
          stats: { ticks: 0, skipped_already_dispatched: 0 },
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
    return suffixFromMarker(obs && obs.key, marker) || suffixFromMarker(obs && obs.evidence_ref, marker);
  }

  function dispatchedAt(obs, nowIso) {
    const value = (obs && obs.value) || {};
    return value.dispatched_at || value.timestamp || obs.timestamp || nowIso;
  }

  function tick(ctx) {
    const nowIso = new Date(ctx.now).toISOString();
    const observations = Array.isArray(ctx.observations) ? ctx.observations : [];
    const checkpoint = initCheckpoint(ctx.checkpoint);
    checkpoint.stats.ticks += 1;

    const dispatched = new Map();
    for (const obs of observations) {
      const signature = markerSignature(obs, "candidate_dispatched");
      if (signature) {
        dispatched.set(signature, dispatchedAt(obs, nowIso));
      }
    }
    for (const [signature, timestamp] of dispatched.entries()) {
      checkpoint.dispatched_signatures[signature] = timestamp;
    }

    for (const obs of observations) {
      const signature = markerSignature(obs, "candidate_approved");
      if (!signature) {
        continue;
      }
      if (dispatched.has(signature) || checkpoint.dispatched_signatures[signature]) {
        checkpoint.stats.skipped_already_dispatched += 1;
        return {
          action: "complete",
          checkpoint,
          result: { approved_count: 1 },
        };
      }

      return {
        action: "agent",
        prompt: [
          `Create or update a GitHub Issue for automation candidate ${signature}.`,
          `Write routine_observation:candidate_dispatched:${signature} after dispatch.`,
        ].join("\n"),
        checkpoint,
        result: { approved_count: 1 },
      };
    }

    return {
      action: "complete",
      checkpoint,
      result: { approved_count: 0 },
    };
  }

  agentdesk.routines.register({ id: "fixture-automation-executor", tick });
})();
