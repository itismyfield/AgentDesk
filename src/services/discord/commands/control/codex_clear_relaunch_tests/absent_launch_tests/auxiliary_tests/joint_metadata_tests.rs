use super::*;
use crate::services::discord::commands::control::codex_clear_relaunch_tests::CanaryFixture;
use crate::services::tui_o::store::rotation::{Boundary, Rotation, SourceLink};
use crate::services::tui_o::store::spool::{Cursor, source_key};
use crate::services::tui_o::store::{InitSource, Initialized, OEra, OStore, StoreConfig};
use crate::services::tui_prompt_dedupe::binding_events as p5;

pub(in crate::services::discord) struct OldSourceSnapshot {
    store: OStore,
    era: OEra,
    source: p5::SourceId,
    cursor: Cursor,
    rotation: Rotation,
    cursor_path: PathBuf,
    cursor_bytes: Vec<u8>,
    body: Vec<u8>,
}

impl OldSourceSnapshot {
    pub(in crate::services::discord) fn seed(fixture: &CanaryFixture) -> Self {
        let body = fs::read(&fixture.old_source.path).unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, fixture.root.path())
            .unwrap()
            .unwrap();
        let era = store
            .begin_era(&[CANARY_CHANNEL], chrono::Utc::now(), |channel| {
                Ok(Initialized {
                    channel,
                    sources: vec![InitSource {
                        source_id: fixture.old_source.clone(),
                        delivery_start: body.len() as u64,
                        prefix_hash: hex::encode(Sha256::digest(&body)),
                    }],
                    initial_anchor: 0,
                    build_digest: "bundle-no-core-old-cursor".into(),
                    at: chrono::Utc::now(),
                })
            })
            .unwrap();
        let mut channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
        channel.set_retired(&fixture.old_source, false).unwrap();
        let proof = p5::codex::read_ownership(&fixture.context)
            .unwrap()
            .verified
            .unwrap();
        let mut rotation = channel.rotation().unwrap();
        rotation.links.insert(
            source_key(&fixture.old_source),
            SourceLink {
                source: fixture.old_source.clone(),
                seq: proof.seq,
                parent: None,
                committed_at: fixture.context.created_at,
                boundary: Boundary::Owed {
                    from: body.len() as u64,
                },
                hot_candidate: None,
            },
        );
        channel.write_rotation(&rotation).unwrap();
        channel.set_binding_checkpoint(proof.seq).unwrap();
        let cursor = channel.cursor(&fixture.old_source).unwrap().clone();
        let cursor_path = fixture
            .root
            .path()
            .join("o_store")
            .join(CANARY_CHANNEL.to_string())
            .join("cursor")
            .join(format!("{}.json", source_key(&fixture.old_source)));
        let cursor_bytes = fs::read(&cursor_path).unwrap();
        Self {
            store,
            era,
            source: fixture.old_source.clone(),
            cursor,
            rotation,
            cursor_path,
            cursor_bytes,
            body,
        }
    }

    pub(in crate::services::discord) fn assert_preserved(&self) {
        let channel = self
            .store
            .open_channel(&self.era, CANARY_CHANNEL)
            .unwrap()
            .unwrap();
        let key = source_key(&self.source);
        assert_eq!(channel.cursor(&self.source), Some(&self.cursor));
        assert_eq!(
            channel.rotation().unwrap().links.get(&key),
            self.rotation.links.get(&key)
        );
        assert_eq!(
            channel.rotation().unwrap().successors.get(&key),
            self.rotation.successors.get(&key)
        );
        assert_eq!(fs::read(&self.cursor_path).unwrap(), self.cursor_bytes);
        assert_eq!(fs::read(&self.source.path).unwrap(), self.body);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn no_core_missing_metadata_preserves_restored_fifo_pg() {
    if isolated(concat!(
        module_path!(),
        "::no_core_missing_metadata_preserves_restored_fifo_pg"
    )) {
        missing_proof_holds(Evidence {
            registry: true,
            routine: true,
            no_core: true,
            metadata: Metadata::Missing,
        })
        .await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn no_core_conflicting_metadata_identity_preserves_restored_fifo_pg() {
    if isolated(concat!(
        module_path!(),
        "::no_core_conflicting_metadata_identity_preserves_restored_fifo_pg"
    )) {
        missing_proof_holds(Evidence {
            registry: true,
            routine: true,
            no_core: true,
            metadata: Metadata::Conflict,
        })
        .await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn no_core_missing_registered_owner_preserves_restored_fifo_pg() {
    if isolated(concat!(
        module_path!(),
        "::no_core_missing_registered_owner_preserves_restored_fifo_pg"
    )) {
        missing_proof_holds(Evidence {
            registry: false,
            routine: true,
            no_core: true,
            metadata: Metadata::Exact,
        })
        .await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn no_core_missing_routine_receipt_preserves_restored_fifo_pg() {
    if isolated(concat!(
        module_path!(),
        "::no_core_missing_routine_receipt_preserves_restored_fifo_pg"
    )) {
        missing_proof_holds(Evidence {
            registry: true,
            routine: false,
            no_core: true,
            metadata: Metadata::Exact,
        })
        .await;
    }
}
