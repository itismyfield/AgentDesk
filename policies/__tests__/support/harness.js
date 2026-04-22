const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const REPO_ROOT = path.resolve(__dirname, "..", "..", "..");

function clone(value) {
  if (value === undefined) return undefined;
  return JSON.parse(JSON.stringify(value));
}

function toPlain(value) {
  return clone(value);
}

function defaultPipelineConfig() {
  return {
    states: [
      { id: "backlog" },
      { id: "requested" },
      { id: "in_progress" },
      { id: "review" },
      { id: "done", terminal: true }
    ],
    transitions: [
      { from: "backlog", to: "requested", type: "free" },
      { from: "requested", to: "in_progress", type: "free" },
      { from: "in_progress", to: "review", type: "gated" },
      { from: "review", to: "done", type: "gated", gates: ["review_passed"] },
      { from: "review", to: "in_progress", type: "gated", gates: ["review_rework"] }
    ]
  };
}

function createPipeline(config) {
  const resolved = clone(config || defaultPipelineConfig());
  const transitions = resolved.transitions || [];
  const states = resolved.states || [];

  return {
    resolveForCard() {
      return resolved;
    },
    getConfig() {
      return resolved;
    },
    kickoffState() {
      return "requested";
    },
    terminalState() {
      const terminal = states.find((state) => state.terminal);
      return terminal ? terminal.id : "done";
    },
    hasState(stateId) {
      return states.some((state) => state.id === stateId);
    },
    nextGatedTarget(from) {
      const transition = transitions.find((item) => item.from === from);
      return transition ? transition.to : null;
    },
    nextGatedTargetWithGate(from, gate) {
      const transition = transitions.find(
        (item) => item.from === from && Array.isArray(item.gates) && item.gates.includes(gate)
      );
      return transition ? transition.to : null;
    },
    isTerminal(status) {
      return status === this.terminalState();
    },
    resolvePhaseGateForCard() {
      return {
        dispatch_to: "self",
        dispatch_type: "phase-gate",
        pass_verdict: "phase_gate_passed",
        checks: []
      };
    }
  };
}

function createSqlRouter(routes) {
  return function routeSql(sql, params) {
    for (const route of routes) {
      const match = route.match;
      const matched =
        typeof match === "string" ? sql.includes(match) :
        match instanceof RegExp ? match.test(sql) :
        typeof match === "function" ? match(sql, params) :
        false;
      if (!matched) continue;
      const result = typeof route.result === "function" ? route.result(sql, params) : route.result;
      return clone(result);
    }
    throw new Error(`Unhandled SQL query: ${sql} :: ${JSON.stringify(params || [])}`);
  };
}

function createExecRouter(routes) {
  return function execRouter(cmd, args) {
    for (const route of routes) {
      const match = route.match;
      const matched =
        typeof match === "string" ? cmd === match :
        match instanceof RegExp ? match.test(cmd) :
        typeof match === "function" ? match(cmd, args) :
        false;
      if (!matched) continue;
      return typeof route.result === "function" ? route.result(cmd, args) : route.result;
    }
    throw new Error(`Unhandled exec call: ${cmd} ${JSON.stringify(args || [])}`);
  };
}

function createAgentdeskMock(options) {
  const settings = options || {};
  const pipeline = settings.pipeline || createPipeline(settings.pipelineConfig);
  const state = {
    registeredPolicies: [],
    logs: { info: [], warn: [], error: [] },
    executions: [],
    statusCalls: [],
    reviewStatusCalls: [],
    reviewStateSyncs: [],
    reviewRecordCalls: [],
    dispatchCreates: [],
    dispatchMarkCompletedCalls: [],
    messageQueues: [],
    autoQueueStatusUpdates: [],
    autoQueueActivations: [],
    autoQueueCompletes: [],
    autoQueuePauses: [],
    autoQueueResumes: [],
    autoQueueSavedPhaseGates: [],
    autoQueueClearedPhaseGates: [],
    retrospectiveCalls: [],
    kv: new Map()
  };

  const dbQuery = settings.dbQuery || (() => []);
  const dbExecute = settings.dbExecute || (() => ({ changes: 1 }));
  const exec = settings.exec || (() => "");
  const cards = settings.cards || {};
  const counterChannels = settings.counterChannels || {};
  const primaryChannels = settings.primaryChannels || {};
  const configValues = Object.assign(
    {
      review_enabled: true,
      max_review_rounds: 3,
      pm_decision_gate_enabled: true,
      maxEntryRetries: 3
    },
    settings.config || {}
  );

  const agentdesk = {
    registerPolicy(policy) {
      state.registeredPolicies.push(policy);
    },
    db: {
      query(sql, params) {
        return clone(dbQuery(sql, params || [], state));
      },
      execute(sql, params) {
        state.executions.push({ sql, params: params || [] });
        return dbExecute(sql, params || [], state);
      }
    },
    exec(cmd, args, execOptions) {
      return exec(cmd, args || [], execOptions || {}, state);
    },
    config: {
      get(key) {
        return Object.prototype.hasOwnProperty.call(configValues, key) ? configValues[key] : null;
      }
    },
    pipeline,
    kanban: {
      setStatus(cardId, status, force) {
        state.statusCalls.push({ cardId, status, force: !!force });
      },
      setReviewStatus(cardId, reviewStatus, optionsArg) {
        state.reviewStatusCalls.push({ cardId, reviewStatus, options: clone(optionsArg || {}) });
      }
    },
    reviewState: {
      sync(cardId, status, optionsArg) {
        state.reviewStateSyncs.push({ cardId, status, options: clone(optionsArg || {}) });
      }
    },
    review: {
      entryContext(cardId) {
        if (typeof settings.reviewEntryContext === "function") {
          return settings.reviewEntryContext(cardId, state);
        }
        return clone(settings.reviewEntryContext || {
          current_round: 0,
          completed_work_count: 1,
          should_advance_round: true,
          next_round: 1
        });
      },
      hasActiveWork(cardId) {
        if (typeof settings.hasActiveWork === "function") {
          return settings.hasActiveWork(cardId, state);
        }
        return !!settings.hasActiveWork;
      },
      recordEntry(cardId, optionsArg) {
        state.reviewRecordCalls.push({ cardId, options: clone(optionsArg || {}) });
      }
    },
    cards: {
      get(cardId) {
        return clone(cards[cardId] || null);
      }
    },
    agents: {
      resolveCounterModelChannel(agentId) {
        return Object.prototype.hasOwnProperty.call(counterChannels, agentId) ? counterChannels[agentId] : null;
      },
      resolvePrimaryChannel(agentId) {
        return Object.prototype.hasOwnProperty.call(primaryChannels, agentId) ? primaryChannels[agentId] : null;
      }
    },
    dispatch: {
      create(cardId, agentId, dispatchType, title, context) {
        if (typeof settings.dispatchCreate === "function") {
          return settings.dispatchCreate(cardId, agentId, dispatchType, title, context, state);
        }
        state.dispatchCreates.push({ cardId, agentId, dispatchType, title, context: clone(context || null) });
        return `dispatch-${state.dispatchCreates.length}`;
      },
      markCompleted(dispatchId, result) {
        state.dispatchMarkCompletedCalls.push({ dispatchId, result });
        if (typeof settings.markCompleted === "function") {
          return settings.markCompleted(dispatchId, result, state);
        }
        return { rows_affected: 1 };
      }
    },
    message: {
      queue(target, content, bot, source) {
        state.messageQueues.push({ target, content, bot, source });
      }
    },
    log: {
      info(message) {
        state.logs.info.push(String(message));
      },
      warn(message) {
        state.logs.warn.push(String(message));
      },
      error(message) {
        state.logs.error.push(String(message));
      }
    },
    runtime: {
      recordCardRetrospective(cardId, status) {
        state.retrospectiveCalls.push({ cardId, status });
        return null;
      },
      refreshInventoryDocs() {}
    },
    kv: {
      get(key) {
        return state.kv.has(key) ? state.kv.get(key) : null;
      },
      set(key, value) {
        state.kv.set(key, value);
      },
      delete(key) {
        state.kv.delete(key);
      }
    },
    autoQueue: {
      updateEntryStatus(entryId, status, reason, extra) {
        state.autoQueueStatusUpdates.push({ entryId, status, reason, extra: clone(extra || null) });
      },
      activate(runId, threadGroup) {
        state.autoQueueActivations.push({ runId, threadGroup });
        return { activated: true };
      },
      completeRun(runId, reason, optionsArg) {
        state.autoQueueCompletes.push({ runId, reason, options: clone(optionsArg || {}) });
        return { changed: true };
      },
      pauseRun(runId, source) {
        state.autoQueuePauses.push({ runId, source });
        return { changed: true };
      },
      resumeRun(runId, source) {
        state.autoQueueResumes.push({ runId, source });
        return { changed: true };
      },
      savePhaseGateState(runId, phase, gateState) {
        state.autoQueueSavedPhaseGates.push({ runId, phase, state: clone(gateState) });
      },
      clearPhaseGateState(runId, phase) {
        state.autoQueueClearedPhaseGates.push({ runId, phase });
      },
      recordConsultationDispatch() {},
      recordDispatchFailure(entryId, retryLimit, source) {
        if (typeof settings.recordDispatchFailure === "function") {
          return clone(settings.recordDispatchFailure(entryId, retryLimit, source, state));
        }
        return { retryCount: 1, retryLimit: 3, to: "pending", changed: true };
      }
    },
    prTracking: {
      load(cardId) {
        return clone((settings.prTracking && settings.prTracking.load && settings.prTracking.load(cardId, state)) || null);
      },
      upsert(cardId, repoId, worktreePath, branch, prNumber, headSha, trackingState, lastError) {
        if (settings.prTracking && typeof settings.prTracking.upsert === "function") {
          return settings.prTracking.upsert(
            cardId,
            repoId,
            worktreePath,
            branch,
            prNumber,
            headSha,
            trackingState,
            lastError,
            state
          );
        }
        return { card_id: cardId, repo_id: repoId, branch, pr_number: prNumber, state: trackingState, last_error: lastError };
      },
      findOpenPrByBranch(repoId, branch) {
        if (settings.prTracking && typeof settings.prTracking.findOpenPrByBranch === "function") {
          return clone(settings.prTracking.findOpenPrByBranch(repoId, branch, state));
        }
        return null;
      },
      extractRepoFromIssueUrl(url) {
        const match = String(url || "").match(/github\.com\/([^/]+\/[^/]+)/);
        return match ? match[1] : null;
      }
    },
    reviewAutomation: {}
  };

  if (settings.extraAgentdesk && typeof settings.extraAgentdesk === "object") {
    Object.assign(agentdesk, settings.extraAgentdesk);
  }

  return { agentdesk, state };
}

function loadPolicy(relativePath, options) {
  const absPath = path.join(REPO_ROOT, relativePath);
  const source = fs.readFileSync(absPath, "utf8");
  const { agentdesk, state } = createAgentdeskMock(options);
  const globals = Object.assign(
    {
      loadLoopGuardRecord() {
        return {};
      },
      replaceLoopGuardRecord(_cardId, _guardType, nextValue) {
        return nextValue;
      },
      loopGuardNowMs() {
        return Date.now();
      },
      loopGuardNowIso() {
        return new Date().toISOString();
      },
      LOOP_GUARD_TTL_SEC: 3600,
      notifyDeadlockManager() {},
      notifyHumanAlert() {
        return true;
      },
      escalate() {},
      escalateToManualIntervention() {}
    },
    (options && options.globals) || {}
  );
  const context = vm.createContext({
    agentdesk,
    module: { exports: {} },
    exports: {},
    console,
    Date,
    JSON,
    Math,
    Object,
    Array,
    String,
    Number,
    Boolean,
    RegExp,
    Error,
    parseInt,
    isFinite,
    ...globals
  });

  vm.runInContext(source, context, { filename: absPath });

  return {
    agentdesk,
    state,
    module: context.module.exports || {},
    policy: (context.module.exports && context.module.exports.policy) || state.registeredPolicies[0],
    context
  };
}

module.exports = {
  createSqlRouter,
  createExecRouter,
  createPipeline,
  defaultPipelineConfig,
  loadPolicy,
  toPlain
};
