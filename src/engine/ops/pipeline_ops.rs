use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};

// ── Pipeline ops ─────────────────────────────────────────────────
//
// Exposes pipeline config to JS policies so they can look up transitions,
// terminal states, etc. instead of hardcoding state names.

pub(super) fn register_pipeline_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let pipeline_obj = Object::new(ctx.clone())?;

    // __getConfigRaw(): returns the full default pipeline config as JSON
    pipeline_obj.set(
        "__getConfigRaw",
        Function::new(ctx.clone(), || -> String {
            crate::pipeline::ensure_loaded();
            match crate::pipeline::try_get() {
                Some(p) => {
                    serde_json::to_string(&p.to_json()).unwrap_or_else(|_| "null".to_string())
                }
                None => "null".to_string(),
            }
        })?,
    )?;

    // __resolveForCardRaw(cardId): returns the effective pipeline for a card
    let db_resolve = db;
    pipeline_obj.set(
        "__resolveForCardRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            crate::pipeline::ensure_loaded();
            let conn = match db_resolve.separate_conn() {
                Ok(c) => c,
                Err(_) => {
                    return crate::pipeline::try_get()
                        .map(|p| {
                            serde_json::to_string(&p.to_json())
                                .unwrap_or_else(|_| "null".to_string())
                        })
                        .unwrap_or_else(|| "null".to_string());
                }
            };
            let repo_id: Option<String> = conn
                .query_row(
                    "SELECT repo_id FROM kanban_cards WHERE id = ?1",
                    [&card_id],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            let agent_id: Option<String> = conn
                .query_row(
                    "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?1",
                    [&card_id],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            let effective =
                crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref());
            serde_json::to_string(&effective.to_json()).unwrap_or_else(|_| "null".to_string())
        })?,
    )?;

    ad.set("pipeline", pipeline_obj)?;

    // JS wrapper with convenience methods
    ctx.eval::<(), _>(r#"
        (function() {
            var rawConfig = agentdesk.pipeline.__getConfigRaw;
            var rawResolve = agentdesk.pipeline.__resolveForCardRaw;

            agentdesk.pipeline.getConfig = function() {
                return JSON.parse(rawConfig());
            };

            agentdesk.pipeline.resolveForCard = function(cardId) {
                return JSON.parse(rawResolve(cardId));
            };

            agentdesk.pipeline.resolvePhaseGate = function(config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                var gate = (cfg && cfg.phase_gate) ? cfg.phase_gate : {};
                var checks = Array.isArray(gate.checks) && gate.checks.length > 0
                    ? gate.checks.slice()
                    : ["merge_verified", "issue_closed", "build_passed"];
                return {
                    dispatch_to: gate.dispatch_to || "self",
                    dispatch_type: gate.dispatch_type || "phase-gate",
                    pass_verdict: gate.pass_verdict || "phase_gate_passed",
                    checks: checks
                };
            };

            agentdesk.pipeline.resolvePhaseGateForCard = function(cardId) {
                return agentdesk.pipeline.resolvePhaseGate(agentdesk.pipeline.resolveForCard(cardId));
            };

            agentdesk.pipeline.isTerminal = function(state, config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.states) return state === "done";
                for (var i = 0; i < cfg.states.length; i++) {
                    if (cfg.states[i].id === state && cfg.states[i].terminal) return true;
                }
                return false;
            };

            agentdesk.pipeline.terminalState = function(config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.states) return "done";
                for (var i = 0; i < cfg.states.length; i++) {
                    if (cfg.states[i].terminal) return cfg.states[i].id;
                }
                return "done";
            };

            agentdesk.pipeline.initialState = function(config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.states) return "backlog";
                for (var i = 0; i < cfg.states.length; i++) {
                    if (!cfg.states[i].terminal) return cfg.states[i].id;
                }
                return "backlog";
            };

            // kickoffState: the first gated-inbound state (dispatch entry, e.g. "requested").
            agentdesk.pipeline.kickoffState = function(config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.states || !cfg.transitions) return "requested";
                for (var si = 0; si < cfg.states.length; si++) {
                    var s = cfg.states[si];
                    if (s.terminal) continue;
                    var hasGatedOut = false, allInboundFree = true;
                    for (var ti = 0; ti < cfg.transitions.length; ti++) {
                        var t = cfg.transitions[ti];
                        if (t.from === s.id && t.type === "gated") hasGatedOut = true;
                        if (t.to === s.id && t.type !== "free") allInboundFree = false;
                    }
                    if (hasGatedOut && allInboundFree) {
                        for (var ti2 = 0; ti2 < cfg.transitions.length; ti2++) {
                            var t2 = cfg.transitions[ti2];
                            if (t2.from === s.id && t2.type === "gated") return t2.to;
                        }
                    }
                }
                return "requested";
            };

            agentdesk.pipeline.findTransition = function(from, to, config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.transitions) return null;
                for (var i = 0; i < cfg.transitions.length; i++) {
                    var t = cfg.transitions[i];
                    if (t.from === from && t.to === to) return t;
                }
                return null;
            };

            agentdesk.pipeline.nextGatedTarget = function(from, config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.transitions) return null;
                for (var i = 0; i < cfg.transitions.length; i++) {
                    var t = cfg.transitions[i];
                    if (t.from === from && t.type === "gated") return t.to;
                }
                return null;
            };

            agentdesk.pipeline.nextGatedTargetWithGate = function(from, gateName, config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.transitions) return null;
                for (var i = 0; i < cfg.transitions.length; i++) {
                    var t = cfg.transitions[i];
                    if (t.from === from && t.type === "gated" && t.gates && t.gates.indexOf(gateName) >= 0) {
                        return t.to;
                    }
                }
                return null;
            };

            agentdesk.pipeline.forceOnlyTargets = function(from, config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.transitions) return [];
                var targets = [];
                for (var i = 0; i < cfg.transitions.length; i++) {
                    var t = cfg.transitions[i];
                    if (t.from === from && t.type === "force_only") targets.push(t.to);
                }
                return targets;
            };

            agentdesk.pipeline.getTimeout = function(state, config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.timeouts) return null;
                return cfg.timeouts[state] || null;
            };

            agentdesk.pipeline.hasState = function(state, config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.states) return false;
                for (var i = 0; i < cfg.states.length; i++) {
                    if (cfg.states[i].id === state) return true;
                }
                return false;
            };

            agentdesk.pipeline.dispatchableStates = function(config) {
                var cfg = config || agentdesk.pipeline.getConfig();
                if (!cfg || !cfg.states) return [];
                var result = [];
                for (var i = 0; i < cfg.states.length; i++) {
                    if (cfg.states[i].dispatchable) result.push(cfg.states[i].id);
                }
                return result;
            };
        })();
    "#)?;

    Ok(())
}
