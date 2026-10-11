use super::permit::{EffectTarget, PermitRefusal, PreparedRetryStart, RetryPermit, StartPermit};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::replay_disposition::write::{
    self, BeginAck, BeginFrom, CanonicalInput, EffectProjection, NoEffectAck,
};
use std::cell::Cell;
use std::marker::PhantomData;

// Inherent consts win over the trait fallback only when the bound holds, so each flag reads a real impl.
trait Fallback {
    const CLONE: bool = false;
    const SERIALIZE: bool = false;
    const DESERIALIZE: bool = false;
}
struct CloneProbe<T>(PhantomData<T>);
impl<T> Fallback for CloneProbe<T> {}
impl<T: Clone> CloneProbe<T> {
    const CLONE: bool = true;
}
struct SerializeProbe<T>(PhantomData<T>);
impl<T> Fallback for SerializeProbe<T> {}
impl<T: serde::Serialize> SerializeProbe<T> {
    const SERIALIZE: bool = true;
}
struct DeserializeProbe<T>(PhantomData<T>);
impl<T> Fallback for DeserializeProbe<T> {}
impl<T: serde::de::DeserializeOwned> DeserializeProbe<T> {
    const DESERIALIZE: bool = true;
}

macro_rules! copyable {
    ($ty:ty) => {
        (
            CloneProbe::<$ty>::CLONE,
            SerializeProbe::<$ty>::SERIALIZE,
            DeserializeProbe::<$ty>::DESERIALIZE,
        )
    };
}

#[test]
fn effect_permits_cannot_be_cloned_or_serialised() {
    assert_eq!(
        copyable!(String),
        (true, true, true),
        "the probes detect real impls"
    );
    for (name, flags) in [
        ("StartPermit", copyable!(StartPermit)),
        ("RetryPermit", copyable!(RetryPermit)),
        ("PreparedRetryStart", copyable!(PreparedRetryStart<String>)),
        ("BeginAck", copyable!(BeginAck)),
        ("NoEffectAck", copyable!(NoEffectAck)),
    ] {
        assert_eq!(flags, (false, false, false), "{name} must stay move-only");
    }
}

fn provider(channel: &str) -> EffectTarget {
    EffectTarget::ProviderStart {
        provider: "claude".into(),
        channel: channel.into(),
    }
}

async fn begun(pool: &sqlx::PgPool, key: &str, target: &EffectTarget) -> BeginAck {
    let request = CanonicalInput {
        request_key: key.into(),
        provider: "claude".into(),
        channel: "chan".into(),
        sources: vec![format!("{key}-source")],
        original_text: "original".into(),
        owner_id: "user".into(),
        agent_id: "agent".into(),
        attachments: serde_json::json!([]),
        reply_context: None,
        provenance: "new_input",
    };
    let receipt = write::register_or_reuse(pool, &request, "node-a")
        .await
        .unwrap();
    let nonce = receipt.episode_nonce.unwrap();
    let key = target.key();
    let binding = serde_json::json!({});
    let effect = EffectProjection {
        effect_target: &key,
        input_hash: "prepared-input",
        binding: &binding,
    };
    let from = BeginFrom::Registered { nonce: &nonce };
    write::begin(pool, receipt.id, from, &effect, "incarnation-a")
        .await
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn start_permit_runs_one_effect_only_on_its_own_target_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(4).await;
    let target = provider("chan");
    let pane = EffectTarget::BusyPane {
        owner: "owner".into(),
        pane: "chan".into(),
    };
    assert_ne!(target.key(), pane.key());
    let ack = begun(&pool, "sealed-for-other", &target).await;
    assert_eq!(
        StartPermit::seal(ack, &pane).err(),
        Some(PermitRefusal::TargetMismatch)
    );

    let effects = Cell::new(0);
    let run = |_| effects.set(effects.get() + 1);
    let permit = StartPermit::seal(begun(&pool, "wrong-target", &target).await, &target).unwrap();
    assert_eq!(
        permit.consume(&pane, "prepared-input", run).err(),
        Some(PermitRefusal::TargetMismatch)
    );
    let permit = StartPermit::seal(begun(&pool, "wrong-input", &target).await, &target).unwrap();
    assert_eq!(
        permit.consume(&target, "history-prompt", run).err(),
        Some(PermitRefusal::InputMismatch)
    );
    assert_eq!(effects.get(), 0, "a refused permit runs no effect");

    let permit = StartPermit::seal(begun(&pool, "spent", &target).await, &target).unwrap();
    let receipt_id = permit.attempt().receipt_id();
    let spent = permit.consume(&target, "prepared-input", |attempt| {
        effects.set(effects.get() + 1);
        attempt.receipt_id()
    });
    assert_eq!((spent, effects.get()), (Ok(receipt_id), 1));
    let fresh = StartPermit::seal(begun(&pool, "not-a-retry", &target).await, &target).unwrap();
    assert_eq!(
        PreparedRetryStart::new("fresh args", fresh).err(),
        Some(PermitRefusal::NotARetry),
        "a first-start permit is not a retry"
    );
    pool.close().await;
    fixture.drop().await;
}
