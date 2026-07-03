(function () {
  const MAX_ITERATIONS = 10;
  const DISPATCH_RETRY_MS = 30 * 60 * 1000;
  const MAX_DISPATCH_RETRIES = 3;

  function initCheckpoint(checkpoint) {
    return checkpoint && checkpoint.version === 2
      ? JSON.parse(JSON.stringify(checkpoint))
      : {
          version: 2,
          dispatched: {},
          pending: {},
          stats: { ticks: 0, dispatched: 0, skipped: 0, max_iterations_reached: 0 },
        };
  }

  function ensureStats(checkpoint) {
    checkpoint.stats = checkpoint.stats || {};
    for (const key of ["ticks", "dispatched", "skipped", "max_iterations_reached"]) {
      checkpoint.stats[key] = checkpoint.stats[key] || 0;
    }
  }

  function dispatchedCardId(obs) {
    if (!obs || obs.source !== "kanban_dispatched") return null;
    if (obs.card_id) return obs.card_id;
    const prefix = "kanban_cards:";
    return typeof obs.evidence_ref === "string" && obs.evidence_ref.startsWith(prefix)
      ? obs.evidence_ref.slice(prefix.length)
      : null;
  }

  function isAutomationCandidate(obs) {
    return Boolean(
      obs &&
      obs.source === "kanban_ready" &&
      (
        obs.pipeline_stage_id === "automation-candidate" ||
        (obs.metadata && obs.metadata.automation_candidate)
      )
    );
  }

  function hasCompleteProgram(obs) {
    const program = obs && obs.metadata && obs.metadata.program;
    return Boolean(
      program &&
      program.repo_dir &&
      program.description &&
      Array.isArray(program.allowed_write_paths) &&
      program.allowed_write_paths.length > 0 &&
      program.metric_name &&
      program.metric_target !== undefined
    );
  }

  function previousIterations(inventory, cardId) {
    if (!inventory) return [];
    if (Array.isArray(inventory)) {
      const rows = [];
      for (const item of inventory) {
        if (!item || item.card_id !== cardId) continue;
        if (Array.isArray(item.iterations)) rows.push(...item.iterations);
        else rows.push(item);
      }
      return rows;
    }
    if (inventory.card_id === cardId && Array.isArray(inventory.iterations)) {
      return inventory.iterations;
    }
    const keyed = inventory[cardId];
    if (Array.isArray(keyed)) return keyed;
    if (keyed && Array.isArray(keyed.iterations)) return keyed.iterations;
    return [];
  }

  function buildPrompt(obs, iteration, history) {
    const program = obs.metadata.program;
    const branch = `automation/${obs.card_id}/iter-${iteration}`;
    const lines = [
      `Run automation candidate ${obs.card_id}.`,
      `Branch: ${branch}`,
      `Iteration: ${iteration} / ${MAX_ITERATIONS}`,
      `Repo: ${program.repo_dir}`,
      `Description: ${program.description}`,
      `allowed_write_paths: ${program.allowed_write_paths.join(", ")}`,
      `metric_name: ${program.metric_name}`,
      `metric_target: ${program.metric_target}`,
      `Submit: /api/automation-candidates/${obs.card_id}/iteration-result`,
    ];
    for (const item of history) {
      lines.push(`Previous: ${item.iteration || ""} ${item.status || ""} ${item.metric_before || ""} ${item.metric_after || ""} ${item.description || ""}`);
    }
    return lines.join("\n");
  }

  function tick(ctx) {
    const now = new Date(ctx.now);
    const nowIso = now.toISOString();
    const observations = Array.isArray(ctx.observations) ? ctx.observations : [];
    const checkpoint = initCheckpoint(ctx.checkpoint);
    ensureStats(checkpoint);
    checkpoint.stats.ticks += 1;

    const dispatchedObs = new Set();
    for (const obs of observations) {
      const cardId = dispatchedCardId(obs);
      if (cardId) dispatchedObs.add(cardId);
    }

    for (const obs of observations) {
      if (obs.source !== "kanban_ready") continue;
      const cardId = obs.card_id;
      if (!isAutomationCandidate(obs) || !hasCompleteProgram(obs)) {
        checkpoint.stats.skipped += 1;
        continue;
      }
      if (dispatchedObs.has(cardId) || checkpoint.dispatched[cardId]) {
        checkpoint.stats.skipped += 1;
        continue;
      }

      const currentIteration = obs.metadata.program.current_iteration || 0;
      const nextIteration = currentIteration + 1;
      if (nextIteration > MAX_ITERATIONS) {
        checkpoint.dispatched[cardId] = {
          status: "max_iterations_reached",
          iteration: nextIteration,
          dispatched_at: nowIso,
        };
        checkpoint.stats.max_iterations_reached += 1;
        continue;
      }

      const pending = checkpoint.pending[cardId];
      if (pending) {
        if ((pending.attempt_count || 0) >= MAX_DISPATCH_RETRIES) {
          checkpoint.stats.skipped += 1;
          continue;
        }
        const elapsed = now.getTime() - Date.parse(pending.last_attempted_at);
        if (Number.isFinite(elapsed) && elapsed < DISPATCH_RETRY_MS) {
          checkpoint.stats.skipped += 1;
          continue;
        }
      }

      checkpoint.pending[cardId] = {
        first_attempted_at: pending && pending.first_attempted_at ? pending.first_attempted_at : nowIso,
        last_attempted_at: nowIso,
        attempt_count: pending ? (pending.attempt_count || 0) + 1 : 1,
        iteration: nextIteration,
      };
      checkpoint.stats.dispatched += 1;

      return {
        action: "agent",
        prompt: buildPrompt(obs, nextIteration, previousIterations(ctx.automationInventory, cardId)),
        checkpoint,
        result: { dispatched_card_id: cardId },
      };
    }

    return {
      action: "complete",
      checkpoint,
      result: { summary: "\uc5c6\uc74c" },
    };
  }

  agentdesk.routines.register({ id: "fixture-automation-candidate-executor", tick });
})();
