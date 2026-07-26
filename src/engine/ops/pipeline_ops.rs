use rquickjs::{Ctx, Function, Object, Result as JsResult};
use sqlx::{PgPool, Row as SqlxRow};

// ── Pipeline ops ─────────────────────────────────────────────────
//
// Exposes pipeline config to JS policies so they can look up transitions,
// terminal states, etc. instead of hardcoding state names.

pub(super) fn register_pipeline_ops<'js>(ctx: &Ctx<'js>, pg_pool: Option<PgPool>) -> JsResult<()> {
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

    // __resolvePhaseGateDeclarationRaw(kind): returns the canonical immutable declaration.
    pipeline_obj.set(
        "__resolvePhaseGateDeclarationRaw",
        Function::new(ctx.clone(), move |kind: String| -> String {
            let kind = kind.trim();
            let kind = if kind.is_empty() {
                crate::phase_gate::DEFAULT_PHASE_GATE_KIND
            } else {
                kind
            };
            crate::phase_gate::resolve_declaration_value(kind)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "null".to_string())
        })?,
    )?;

    // __resolveForCardRaw(cardId): returns the effective pipeline for a card
    let pg_resolve = pg_pool;
    pipeline_obj.set(
        "__resolveForCardRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            crate::pipeline::ensure_loaded();
            if let Some(pool) = pg_resolve.as_ref() {
                return resolve_for_card_raw_pg(pool, &card_id);
            }
            crate::pipeline::try_get()
                .map(|p| serde_json::to_string(&p.to_json()).unwrap_or_else(|_| "null".to_string()))
                .unwrap_or_else(|| "null".to_string())
        })?,
    )?;

    ad.set("pipeline", pipeline_obj)?;

    // JS wrapper with convenience methods
    ctx.eval::<(), _>(r#"
        (function() {
            agentdesk.pipeline.getConfig = function() {
                return JSON.parse(agentdesk.pipeline.__getConfigRaw());
            };
            // Backward-compatible alias used by older policies.
            agentdesk.pipeline.config = agentdesk.pipeline.getConfig;

            agentdesk.pipeline.resolveForCard = function(cardId) {
                return JSON.parse(agentdesk.pipeline.__resolveForCardRaw(cardId));
            };

            agentdesk.pipeline.resolvePhaseGateDeclaration = function(kind) {
                return JSON.parse(agentdesk.pipeline.__resolvePhaseGateDeclarationRaw(kind || ""));
            };

            agentdesk.pipeline.resolvePhaseGate = function(config, kind) {
                var cfg = config || agentdesk.pipeline.getConfig();
                var gate = (cfg && cfg.phase_gate) ? cfg.phase_gate : {};
                var declaration = agentdesk.pipeline.resolvePhaseGateDeclaration(kind);
                if (!declaration) return null;
                return {
                    dispatch_to: gate.dispatch_to || "self",
                    dispatch_type: gate.dispatch_type || "phase-gate",
                    declaration: declaration
                };
            };

            agentdesk.pipeline.resolvePhaseGateForCard = function(cardId, kind) {
                return agentdesk.pipeline.resolvePhaseGate(agentdesk.pipeline.resolveForCard(cardId), kind);
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

            // kickoffState: the first dispatchable state (dispatch entry, e.g. "requested").
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
                        return s.id;
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

#[cfg(test)]
mod auto_queue_phase_gate_js_contract_tests {
    use super::register_pipeline_ops;
    use crate::phase_gate::PHASE_GATE_DECLARATIONS;
    use rquickjs::{Context, Runtime};

    #[test]
    fn quickjs_phase_gate_declarations_match_rust_registry_serialization() {
        let runtime = Runtime::new().expect("create QuickJS runtime"); // agentdesk-audit: allow-unwrap — test-only QuickJS fixture
        let context = Context::full(&runtime).expect("create QuickJS context"); // agentdesk-audit: allow-unwrap — test-only QuickJS fixture
        context.with(|ctx| {
            let agentdesk = rquickjs::Object::new(ctx.clone()).expect("agentdesk object"); // agentdesk-audit: allow-unwrap — test-only QuickJS fixture
            ctx.globals()
                .set("agentdesk", agentdesk)
                .expect("install agentdesk"); // agentdesk-audit: allow-unwrap — test-only QuickJS fixture
            register_pipeline_ops(&ctx, None).expect("register pipeline ops"); // agentdesk-audit: allow-unwrap — test-only host contract setup

            let catalog = crate::phase_gate::catalog_value();
            let catalog_ids = catalog["kinds"]
                .as_array()
                .expect("catalog kinds") // agentdesk-audit: allow-unwrap — immutable registry catalog fixture
                .iter()
                .map(|kind| kind["id"].as_str().expect("catalog kind id")) // agentdesk-audit: allow-unwrap — catalog entries always include string ids
                .collect::<Vec<_>>();
            let registry_ids = PHASE_GATE_DECLARATIONS
                .iter()
                .map(|declaration| declaration.kind.id())
                .collect::<Vec<_>>();
            assert_eq!(catalog_ids, registry_ids, "catalog/registry kind-set drift");

            for declaration in PHASE_GATE_DECLARATIONS {
                let kind = declaration.kind.id();
                let script = format!(
                    "JSON.stringify(agentdesk.pipeline.resolvePhaseGateDeclaration({kind:?}))"
                );
                let serialized: String = ctx.eval(script).expect("evaluate declaration"); // agentdesk-audit: allow-unwrap — test-only QuickJS assertion
                let from_js: serde_json::Value =
                    serde_json::from_str(&serialized).expect("decode declaration"); // agentdesk-audit: allow-unwrap — host operation must emit valid JSON
                let from_rust =
                    crate::phase_gate::resolve_declaration_value(kind).expect("Rust declaration"); // agentdesk-audit: allow-unwrap — immutable built-in declaration fixture
                assert_eq!(from_js, from_rust, "serialization drift for {kind}");
                for field in [
                    "kind",
                    "declaration_version",
                    "pass_verdict",
                    "evidence_requirement",
                    "required_checks",
                    "available",
                    "unavailable_reason",
                ] {
                    assert_eq!(
                        from_js.get(field),
                        from_rust.get(field),
                        "field drift for {kind}.{field}"
                    );
                }
            }

            let unknown: String = ctx
                .eval("JSON.stringify(agentdesk.pipeline.resolvePhaseGateDeclaration('unknown-gate'))")
                .expect("evaluate unknown declaration"); // agentdesk-audit: allow-unwrap — test-only QuickJS assertion
            assert_eq!(unknown, "null");
            assert!(crate::phase_gate::resolve_declaration_value("unknown-gate").is_none());
        });
    }
}

fn resolve_for_card_raw_pg(pool: &PgPool, card_id: &str) -> String {
    let card_id = card_id.to_string();
    match crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            let row = sqlx::query(
                "SELECT repo_id, assigned_agent_id
                 FROM kanban_cards
                 WHERE id = $1",
            )
            .bind(&card_id)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|error| format!("load postgres card pipeline context {card_id}: {error}"))?;
            let (repo_id, agent_id) = if let Some(row) = row {
                (
                    row.try_get::<Option<String>, _>("repo_id")
                        .map_err(|error| {
                            format!("decode postgres repo_id for {card_id}: {error}")
                        })?,
                    row.try_get::<Option<String>, _>("assigned_agent_id")
                        .map_err(|error| {
                            format!("decode postgres assigned_agent_id for {card_id}: {error}")
                        })?,
                )
            } else {
                (None, None)
            };
            let effective = crate::pipeline::resolve_for_card_pg(
                &bridge_pool,
                repo_id.as_deref(),
                agent_id.as_deref(),
            )
            .await;
            Ok(serde_json::to_string(&effective.to_json()).unwrap_or_else(|_| "null".to_string()))
        },
        |_error| "null".to_string(),
    ) {
        Ok(result) => result,
        Err(result) => result,
    }
}
