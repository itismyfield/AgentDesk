(function () {
  const EVIDENCE_TTL_MS = 25 * 60 * 60 * 1000;
  const PARTIAL_RESET_AGE_MS = 12 * 60 * 60 * 1000;
  const SEEN_EVIDENCE_CAP = 500;

  function initCheckpoint(checkpoint) {
    const cp = checkpoint && checkpoint.version === 1
      ? JSON.parse(JSON.stringify(checkpoint))
      : {
          version: 1,
          cursors: {},
          candidates: {},
          suppressions: {},
          seen_evidence: {},
          recommendations: [],
          last_tick_at: null,
          stats: {
            ticks: 0,
            observations_seen: 0,
            agent_escalations: 0,
            recommendations_today: 0,
            recommendation_day: null,
            category_scored: {},
          },
        };
    cp.candidates = cp.candidates || {};
    cp.suppressions = cp.suppressions || {};
    cp.seen_evidence = cp.seen_evidence || {};
    cp.recommendations = cp.recommendations || [];
    cp.stats = cp.stats || {};
    cp.stats.ticks = cp.stats.ticks || 0;
    cp.stats.observations_seen = cp.stats.observations_seen || 0;
    cp.stats.agent_escalations = cp.stats.agent_escalations || 0;
    cp.stats.recommendations_today = cp.stats.recommendations_today || 0;
    cp.stats.category_scored = cp.stats.category_scored || {};
    cp.ema_scored = cp.ema_scored || 0;
    cp.saturation_ticks = cp.saturation_ticks || 0;
    cp.fast_fail_ticks = cp.fast_fail_ticks || 0;
    cp.reopt_count = cp.reopt_count || 0;
    cp.diversity_mode_ticks_remaining = cp.diversity_mode_ticks_remaining || 0;
    return cp;
  }

  function seenEntry(value) {
    if (!value) return null;
    if (typeof value === "string") {
      return { seen_at: value, occurrences: 1 };
    }
    return {
      seen_at: value.seen_at || value.timestamp,
      occurrences: value.occurrences || 1,
    };
  }

  function evidenceKey(obs) {
    return obs.evidence_ref || `${obs.source || ""}|${obs.category || ""}|${obs.signature || ""}`;
  }

  function dispatchedSignature(obs) {
    const prefix = "routine_observation:candidate_dispatched:";
    if (typeof obs.key === "string" && obs.key.startsWith(prefix)) {
      return obs.key.slice(prefix.length);
    }
    const kvPrefix = `kv_meta:${prefix}`;
    if (typeof obs.evidence_ref === "string" && obs.evidence_ref.startsWith(kvPrefix)) {
      return obs.evidence_ref.slice(kvPrefix.length);
    }
    return null;
  }

  function candidateFor(cp, obs, nowIso) {
    const signature = obs.signature;
    const candidate = cp.candidates[signature] || {
      category: obs.category || "routine-candidate",
      state: "observing",
      score: 0,
      evidence_count: 0,
      first_seen_at: nowIso,
      last_seen_at: nowIso,
      examples: [],
      last_recommended_at: null,
    };
    candidate.category = candidate.category || obs.category || "routine-candidate";
    candidate.last_seen_at = nowIso;
    cp.candidates[signature] = candidate;
    return candidate;
  }

  function gateFor(candidate) {
    return candidate.category === "session-pattern"
      ? { score: 60, evidence: 3 }
      : { score: 80, evidence: 5 };
  }

  function trimSeenEvidence(cp) {
    const entries = Object.entries(cp.seen_evidence);
    if (entries.length <= SEEN_EVIDENCE_CAP) return;
    entries.sort((a, b) => Date.parse(seenEntry(b[1]).seen_at) - Date.parse(seenEntry(a[1]).seen_at));
    cp.seen_evidence = Object.fromEntries(entries.slice(0, SEEN_EVIDENCE_CAP));
  }

  function partialResetSeenEvidence(cp, now) {
    for (const [key, raw] of Object.entries(cp.seen_evidence)) {
      const entry = seenEntry(raw);
      if (!entry || now.getTime() - Date.parse(entry.seen_at) > PARTIAL_RESET_AGE_MS) {
        delete cp.seen_evidence[key];
      }
    }
  }

  function suppressDispatched(cp, observations) {
    const suppressed = [];
    for (const obs of observations) {
      const signature = dispatchedSignature(obs);
      if (signature) {
        delete cp.candidates[signature];
        cp.suppressions[signature] = { reason: "dispatched" };
        suppressed.push(signature);
      }
    }
    return suppressed;
  }

  function maybeEscalate(cp) {
    for (const [signature, candidate] of Object.entries(cp.candidates)) {
      if (cp.suppressions[signature]) continue;
      const gate = gateFor(candidate);
      if ((candidate.score || 0) >= gate.score && (candidate.evidence_count || 0) >= gate.evidence) {
        candidate.state = "recommended";
        cp.stats.agent_escalations += 1;
        cp.recommendations.push({ signature, recommended_at: cp.last_tick_at });
        const prompt = [
          `Review automation candidate ${signature}.`,
          "repo_dir: <determine from your workspace context>",
          `gate=${gate.score}/${gate.evidence}`,
          `Write routine_observation:candidate_review:${signature} if this should proceed.`,
        ].join("\n");
        return { signature, prompt };
      }
    }
    return null;
  }

  function tick(ctx) {
    const now = new Date(ctx.now);
    const nowIso = now.toISOString();
    const observations = Array.isArray(ctx.observations) ? ctx.observations : [];
    const cp = initCheckpoint(ctx.checkpoint);
    cp.stats.ticks += 1;
    cp.stats.observations_seen += observations.length;
    cp.last_tick_at = nowIso;

    if (cp.diversity_mode_ticks_remaining > 0) {
      cp.diversity_mode_ticks_remaining -= 1;
    }

    const suppressed = suppressDispatched(cp, observations);

    let scored = 0;
    let deduped = 0;
    for (const obs of observations) {
      if (!obs || !obs.signature || dispatchedSignature(obs)) continue;
      const key = evidenceKey(obs);
      const occurrences = Math.max(1, Number(obs.occurrences || 1));
      const weight = Math.max(1, Number(obs.weight || 1));
      const seen = seenEntry(cp.seen_evidence[key]);
      const seenAt = seen ? Date.parse(seen.seen_at) : NaN;
      const expired = !seen || !Number.isFinite(seenAt) || now.getTime() - seenAt > EVIDENCE_TTL_MS;

      if (expired) {
        const candidate = candidateFor(cp, obs, nowIso);
        candidate.evidence_count = (candidate.evidence_count || 0) + occurrences;
        candidate.score = Math.min(100, (candidate.score || 0) + occurrences * weight * 10);
        cp.seen_evidence[key] = { seen_at: nowIso, occurrences };
        scored += 1;
        continue;
      }

      if (occurrences > seen.occurrences) {
        const delta = occurrences - seen.occurrences;
        const candidate = candidateFor(cp, obs, nowIso);
        candidate.evidence_count = (candidate.evidence_count || 0) + delta;
        candidate.score = Math.min(100, (candidate.score || 0) + delta * weight * 10);
        cp.seen_evidence[key] = { seen_at: nowIso, occurrences };
        scored += 1;
      } else {
        cp.seen_evidence[key] = { seen_at: nowIso, occurrences };
        deduped += 1;
      }
    }

    trimSeenEvidence(cp);

    cp.ema_scored = cp.ema_scored * 0.9 + scored * 0.1;
    if (observations.length > 0 && scored === 0 && deduped > 0) {
      cp.fast_fail_ticks += 1;
    } else {
      cp.fast_fail_ticks = 0;
    }
    if (cp.ema_scored < 0.3) {
      cp.saturation_ticks += 1;
    } else {
      cp.saturation_ticks = 0;
    }

    let reoptTriggered = null;
    if (cp.fast_fail_ticks >= 2) {
      reoptTriggered = "fast_fail";
    } else if (cp.saturation_ticks >= 5) {
      reoptTriggered = "ema_saturation";
    }
    if (reoptTriggered) {
      cp.reopt_count += 1;
      cp.fast_fail_ticks = 0;
      cp.saturation_ticks = 0;
      cp.diversity_mode_ticks_remaining = 10;
      cp.last_reopt_at = nowIso;
      partialResetSeenEvidence(cp, now);
    }

    const summaryParts = [
      `scored=${scored}`,
      `deduped=${deduped}`,
      `ema_scored=${cp.ema_scored.toFixed(3)}`,
      `saturation_ticks=${cp.saturation_ticks}`,
      `fast_fail_ticks=${cp.fast_fail_ticks}`,
      `reopt_count=${cp.reopt_count}`,
    ];
    if (reoptTriggered) summaryParts.push(`reopt_triggered=${reoptTriggered}`);

    const escalation = maybeEscalate(cp);
    if (escalation) {
      return {
        action: "agent",
        prompt: escalation.prompt,
        checkpoint: cp,
        result: { scoring_summary: summaryParts.join(", "), suppression_summary: suppressed.length ? "dispatched" : "" },
      };
    }

    return {
      action: "complete",
      checkpoint: cp,
      result: { scoring_summary: summaryParts.join(", "), suppression_summary: suppressed.length ? "dispatched" : "" },
    };
  }

  agentdesk.routines.register({ id: "fixture-automation-candidate-recommender", tick });
})();
