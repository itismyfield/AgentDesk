use rquickjs::function::{Args, IntoArgs};
use rquickjs::{Array, Ctx, Exception, FromJs, Function, Object, Result as JsResult, Value};

struct PolicyIo<'js> {
    ctx: Ctx<'js>,
}

impl<'js> PolicyIo<'js> {
    fn call<A: IntoArgs<'js>, R: FromJs<'js>>(
        &self,
        object: &str,
        method: &str,
        args: A,
    ) -> JsResult<R> {
        let ad: Object = self.ctx.globals().get("agentdesk")?;
        let object: Object = ad.get(object)?;
        let function: Function = object.get(method)?;
        let mut call_args = Args::new(self.ctx.clone(), args.num_args());
        call_args.this(object)?;
        args.into_args(&mut call_args)?;
        function.call_arg(call_args)
    }

    // Keep the existing synchronous DB compatibility adapter and JSON projection.
    // This owner alone chooses SQL, thresholds and signal evidence.
    fn query(&self, sql: &str, params: &str) -> JsResult<Array<'js>> {
        let raw: String = self.call("db", "__query_raw", (sql, params))?;
        let result: Value = self.ctx.json_parse(raw)?;
        if let Some(object) = result.as_object()
            && let Some(error) = object.get::<_, Option<String>>("error")?
        {
            return Err(Exception::throw_message(&self.ctx, &error));
        }
        Array::from_js(&self.ctx, result)
    }

    fn text(&self, value: Value<'js>) -> JsResult<String> {
        self.ctx
            .globals()
            .get::<_, Function>("String")?
            .call((value,))
    }
}

pub(crate) fn maintain_policy(ctx: &Ctx<'_>) -> JsResult<()> {
    let io = PolicyIo { ctx: ctx.clone() };
    let boot_rows = io.query(
        "SELECT value FROM kv_meta WHERE key = 'server_boot_at'",
        "[]",
    )?;
    if !boot_rows.is_empty() {
        let row: Object = boot_rows.get(0)?;
        let value: Value = row.get("value")?;
        // Preserve Date's original suffix/coercion/invalid-input behavior.
        let parse: Function =
            ctx.eval("(function(value) { return new Date(value + 'Z').getTime(); })")?;
        let boot_at: f64 = parse.call((value,))?;
        let date: Object = ctx.globals().get("Date")?;
        let now: f64 = date.get::<_, Function>("now")?.call(())?;
        if (now - boot_at) / 60_000.0 < 10.0 {
            return Ok(());
        }
    }

    let config: Value = io.call("pipeline", "getConfig", ())?;
    let initial: Value = io.call("pipeline", "kickoffState", (config.clone(),))?;
    let in_progress: Value = io.call(
        "pipeline",
        "nextGatedTarget",
        (initial.clone(), config.clone()),
    )?;
    let _review: Value = io.call("pipeline", "nextGatedTarget", (in_progress.clone(), config))?;
    let params = Array::new(ctx.clone())?;
    params.set(0, in_progress)?;
    let params = ctx
        .json_stringify(params)?
        .ok_or_else(|| {
            Exception::throw_message(ctx, "orphan query parameters are not serializable")
        })?
        .to_string()?;
    let candidates = io.query(
        "SELECT td.id as dispatch_id, td.kanban_card_id, td.dispatch_type \
         FROM task_dispatches td \
         JOIN kanban_cards kc ON kc.id = td.kanban_card_id \
         WHERE kc.status = ? \
         AND td.status = 'pending' \
         AND kc.latest_dispatch_id = td.id \
         AND td.dispatch_type IN ('implementation', 'rework') \
         AND td.created_at < datetime('now', '-5 minutes') \
         AND NOT EXISTS (\
           SELECT 1 FROM sessions s \
           WHERE s.active_dispatch_id = td.id AND s.status IN ('turn_active', 'working')\
         )",
        &params,
    )?;
    for candidate in candidates.iter::<Object>() {
        let candidate = candidate?;
        let dispatch_id: String = candidate.get("dispatch_id")?;
        let card_id: String = candidate.get("kanban_card_id")?;
        let outcome = (|| -> JsResult<()> {
            let evidence = Object::new(ctx.clone())?;
            evidence.set("dispatch_id", &dispatch_id)?;
            evidence.set("card_id", &card_id)?;
            evidence.set("dispatch_type", candidate.get::<_, Value>("dispatch_type")?)?;
            evidence.set("detected_from", "timeouts._section_K")?;
            // Actor-thread hooks must still defer through the existing intent bridge.
            let decision: Object =
                io.call("runtime", "emitSignal", ("OrphanCandidate", evidence))?;
            if decision.get::<_, bool>("executed")? {
                io.call("log", "warn", (format!(
                    "[orphan-recovery] Supervisor resumed orphaned dispatch {dispatch_id} → card {card_id} → {}",
                    io.text(initial.clone())?
                ),))
            } else {
                let note: Option<String> = decision.get("note")?;
                let suffix = note
                    .filter(|note| !note.is_empty())
                    .map(|note| format!(" — {note}"))
                    .unwrap_or_default();
                io.call(
                    "log",
                    "info",
                    (format!(
                        "[orphan-recovery] Supervisor skipped {dispatch_id}{suffix}"
                    ),),
                )
            }
        })();
        if let Err(error) = outcome {
            let error = if matches!(error, rquickjs::Error::Exception) {
                io.text(ctx.catch())?
            } else {
                error.to_string()
            };
            io.call::<_, ()>(
                "log",
                "error",
                (format!(
                    "[orphan-recovery] Supervisor emit failed for {dispatch_id}: {error}"
                ),),
            )?;
        }
    }
    Ok(())
}
