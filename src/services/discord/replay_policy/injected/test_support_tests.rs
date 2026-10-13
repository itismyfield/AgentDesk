use super::*;
use crate::services::discord::replay_policy::permit::{EffectTarget, StartPermit};
use sqlx::PgPool;

static TRACE: LazyLock<dashmap::DashMap<u64, Vec<&'static str>>> =
    LazyLock::new(dashmap::DashMap::new);
static LOST: LazyLock<dashmap::DashMap<u64, ()>> = LazyLock::new(dashmap::DashMap::new);

pub(crate) fn note(channel: u64, event: &'static str) {
    TRACE.entry(channel).or_default().push(event);
}

pub(crate) fn trace(channel: u64) -> Vec<&'static str> {
    TRACE
        .get(&channel)
        .map(|entry| entry.clone())
        .unwrap_or_default()
}

pub(crate) fn lose_ack(channel: u64) {
    LOST.insert(channel, ());
}

pub(super) fn ack(
    channel: u64,
    result: Result<(), write::WriteError>,
) -> Result<(), write::WriteError> {
    if result.is_ok() && LOST.remove(&channel).is_some() {
        Err(write::WriteError::AckUnknown(sqlx::Error::Io(
            std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "test terminal acknowledgement lost",
            ),
        )))
    } else {
        result
    }
}

pub(crate) struct Input<'a> {
    pub channel: u64,
    pub pane: &'a str,
    pub path: &'a str,
    pub owner_nonce: &'a str,
    pub session: &'a str,
    pub text: &'a str,
    pub frontier: u64,
}

pub(crate) async fn attach(shared: &SharedData, input: Input<'_>) -> i64 {
    let Input {
        channel,
        pane,
        path,
        owner_nonce,
        session,
        text,
        frontier,
    } = input;
    let pool = shared.pg_pool.as_ref().unwrap();
    let suffix = if text == "C-nonce other" {
        "-other"
    } else {
        ""
    };
    let input = write::CanonicalInput {
        request_key: format!("injected-{channel}-{text}"),
        provider: shared.provider.as_str().into(),
        channel: channel.to_string(),
        sources: vec![
            format!("source-{channel}{suffix}"),
            format!("absorbed-{channel}{suffix}"),
        ],
        original_text: text.into(),
        owner_id: "owner-C".into(),
        agent_id: "agent-C".into(),
        attachments: serde_json::json!([{ "id": "C-attachment" }]),
        reply_context: Some("C-reply".into()),
        provenance: "new_input",
    };
    let receipt = write::register_or_reuse(pool, &input, "test-node")
        .await
        .unwrap();
    let nonce = receipt.episode_nonce.as_deref().unwrap();
    let SourceFileIdentity::Unix { dev, ino } =
        SourceFileIdentity::from_open_file(&std::fs::File::open(path).unwrap())
    else {
        panic!("fixture source file");
    };
    let binding = Binding {
        channel,
        provider: shared.provider.as_str().into(),
        pane: pane.into(),
        owner_nonce: owner_nonce.into(),
        session: session.into(),
        injection_nonce: "C-nonce".into(),
        source: Source {
            path: path.into(),
            file: [dev, ino],
            marker: marker(pane).unwrap(),
            reset: shared
                .relay_frontier_token(serenity::all::ChannelId::new(channel))
                .reset_incarnation,
            frontier,
        },
    };
    let target = EffectTarget::BusyPane {
        owner: owner_nonce.into(),
        pane: pane.into(),
    };
    let key = target.key();
    let value = serde_json::to_value(binding).unwrap();
    let hash = input_hash(text);
    let projection = write::EffectProjection {
        effect_target: &key,
        input_hash: &hash,
        binding: &value,
    };
    projection.save(pool, receipt.id, nonce).await.unwrap();
    let begun = write::begin(
        pool,
        receipt.id,
        write::BeginFrom::Registered { nonce },
        &projection,
        "test-incarnation",
    )
    .await
    .unwrap();
    let permit = StartPermit::seal(begun, &target).unwrap();
    permit
        .consume_projected(&target, &hash, |attempt, value, hash| {
            attach_started(attempt, value, hash).unwrap();
        })
        .unwrap();
    receipt.id
}

pub(crate) async fn state(pool: &PgPool, receipt: i64) -> String {
    sqlx::query_scalar("SELECT replay_disposition FROM intake_outbox WHERE id = $1")
        .bind(receipt)
        .fetch_one(pool)
        .await
        .unwrap()
}

pub(crate) async fn withhold(channel: u64, receipt: i64, pool: &PgPool) {
    let entry = INPUTS.get(&(channel, receipt)).unwrap().value().clone();
    let attempt = entry
        .attempt
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .unwrap();
    write::withhold(pool, &attempt, "fixture CAS miss", &serde_json::json!({}))
        .await
        .unwrap();
    *entry
        .attempt
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(attempt);
}

static SWAPS: LazyLock<dashmap::DashMap<u64, (i64, i64)>> = LazyLock::new(dashmap::DashMap::new);

pub(crate) fn wrong_attempt(channel: u64, receipt: i64, other: i64) {
    SWAPS.insert(channel, (receipt, other));
}

pub(super) fn prepare(channel: u64) {
    if let Some((_, (receipt, other))) = SWAPS.remove(&channel) {
        let a = INPUTS.get(&(channel, receipt)).unwrap();
        let b = INPUTS.get(&(channel, other)).unwrap();
        std::mem::swap(
            &mut *a.attempt.lock().unwrap_or_else(|error| error.into_inner()),
            &mut *b.attempt.lock().unwrap_or_else(|error| error.into_inner()),
        );
    }
}

pub(crate) fn reserve(channel: u64, owner_nonce: &str) {
    OWNERS.insert((channel, owner_nonce.into()), ());
}
