//! Policy bridge to the canonical phase-gate declaration and verdict owner.

use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum Request {
    Evaluate {
        context: Option<Value>,
        result: Value,
    },
    AuthoritativeContext {
        run_id: String,
        phase: i64,
        context: Value,
    },
}

pub(super) fn register_phase_gate_ops(ctx: &Ctx<'_>, pool: Option<PgPool>) -> JsResult<()> {
    let ad: Object<'_> = ctx.globals().get("agentdesk")?;
    let owner = Object::new(ctx.clone())?;
    owner.set(
        "__callRaw",
        Function::new(ctx.clone(), move |raw: String| -> String {
            let response = (|| -> Result<Value, String> {
                let request: Request =
                    serde_json::from_str(&raw).map_err(|error| error.to_string())?;
                match request {
                    Request::Evaluate { context, result } => {
                        // Preserve the policy hook's null-result error before it changes gate state.
                        if result.is_null() {
                            return Err("phase-gate result must not be null".into());
                        }
                        Ok(crate::db::auto_queue::phase_gate_verdict::evaluate(
                            context.as_ref(),
                            &result,
                        ))
                    }
                    Request::AuthoritativeContext {
                        run_id,
                        phase,
                        context,
                    } => authoritative_context(pool.as_ref(), run_id, phase, context),
                }
            })();
            match response {
                Ok(value) => json!({"value": value}).to_string(),
                Err(error) => json!({"error": error}).to_string(),
            }
        })?,
    )?;
    ad.set("phaseGate", owner)?;
    ctx.eval::<(), _>(r#"
        (function() {
            function call(payload) {
                var response = JSON.parse(agentdesk.phaseGate.__callRaw(JSON.stringify(payload)));
                if (response.error) throw new Error(response.error);
                return response.value;
            }
            agentdesk.phaseGate.evaluate = function(context, result) {
                return call({op:'evaluate', context:context, result:result});
            };
            agentdesk.phaseGate.authoritativeContext = function(runId, phase, context) {
                return call({op:'authoritativeContext', run_id:runId, phase:phase, context:context});
            };
        })();
    "#)?;
    Ok(())
}

fn authoritative_context(
    pool: Option<&PgPool>,
    run_id: String,
    phase: i64,
    context: Value,
) -> Result<Value, String> {
    if let Some(current) =
        crate::db::auto_queue::phase_gate_verdict::authoritative_context(Some(&context), false)
    {
        return Ok(current);
    }
    if !context
        .get("phase_gate")
        .is_some_and(crate::phase_gate::legacy_context_lacks_declaration_snapshot)
    {
        return Ok(Value::Null);
    }
    let pool = pool.ok_or("postgres backend is required for phase-gate provenance")?;
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |pool| async move {
            let mut tx = pool.begin().await.map_err(|error| error.to_string())?;
            let normalized =
                crate::db::auto_queue::phase_gates::authoritative_phase_gate_context_on_pg_tx(
                    &mut tx,
                    &run_id,
                    phase,
                    Some(&context),
                )
                .await?;
            tx.rollback().await.map_err(|error| error.to_string())?;
            Ok(normalized.unwrap_or(Value::Null))
        },
        |error| error,
    )
}
