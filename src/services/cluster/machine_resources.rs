//! Host telemetry is collected off the heartbeat path. Publishing a cached
//! sample never extends its lifetime or affects execution admission.
use std::sync::{LazyLock, RwLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value};

mod gpu;
mod sampler;
pub(crate) mod store;
#[cfg(test)]
mod tests;

// Five-second sampling keeps the panel responsive without polling processes.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
// Allow three heartbeat opportunities, including slow/default 10-second peers.
const MIN_SAMPLE_TTL: Duration = Duration::from_secs(30);
static SNAPSHOT: LazyLock<RwLock<Option<MachineResources>>> = LazyLock::new(|| RwLock::new(None));

#[derive(Clone, Debug, Serialize)]
pub(crate) struct MachineResources {
    pub schema: u32,
    pub observed_at_ms: i64,
    pub expires_at_ms: i64,
    pub sample_interval_ms: u64,
    pub cpu: CpuResources,
    pub memory: Option<MemoryResources>,
    pub disks: Vec<DiskResources>,
    pub gpus: Vec<GpuResources>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CpuResources {
    pub model: String,
    pub physical_cores: Option<usize>,
    pub logical_cores: usize,
    pub usage_percent: Option<f32>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct MemoryResources {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DiskResources {
    pub name: String,
    pub mount_point: String,
    pub kind: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct GpuResources {
    pub name: String,
    pub usage_percent: Option<f32>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    /// Apple integrated GPUs share system memory; it is not dedicated VRAM.
    pub shared_memory: bool,
}

pub(crate) fn publish(capabilities: &mut Map<String, Value>) {
    capabilities.insert(
        "machine_resources".into(),
        SNAPSHOT
            .read()
            .ok()
            .and_then(|s| s.as_ref().and_then(|s| serde_json::to_value(s).ok()))
            .unwrap_or(Value::Null),
    );
}

pub(crate) fn spawn(heartbeat_interval_secs: u64) {
    let ttl = MIN_SAMPLE_TTL.max(Duration::from_secs(
        heartbeat_interval_secs.saturating_mul(3),
    ));
    tokio::spawn(async move {
        let mut sampler = None;
        let mut gpu = gpu::GpuSampler::default();
        let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            // One collector owns the CPU delta history. Never accumulate
            // detached collectors when a host filesystem is slow.
            let result = tokio::task::spawn_blocking(move || {
                let mut sampler = sampler.unwrap_or_else(sampler::Sampler::new);
                let snapshot = sampler.collect(ttl);
                (sampler, snapshot)
            })
            .await;
            let Ok((next_sampler, mut snapshot)) = result else {
                tracing::warn!("machine resource collector stopped unexpectedly");
                break;
            };
            sampler = Some(next_sampler);
            snapshot.gpus = gpu.collect().await;
            if let Ok(mut cached) = SNAPSHOT.write() {
                *cached = Some(snapshot);
            }
        }
    });
}

fn valid_percent(value: f32) -> Option<f32> {
    (value.is_finite() && value >= 0.0).then(|| value.min(100.0))
}
