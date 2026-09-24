use super::{GpuResources, command::run as command};

#[cfg(any(test, target_os = "macos"))]
mod macos;
#[cfg(not(target_os = "macos"))]
mod other;

#[cfg(any(test, not(target_os = "macos")))]
const MIB_BYTES: f64 = 1024.0 * 1024.0;

#[derive(Default)]
pub(super) struct GpuSampler {
    previous: Vec<GpuResources>,
}

impl GpuSampler {
    pub(super) async fn collect(&mut self) -> Vec<GpuResources> {
        #[cfg(target_os = "macos")]
        let result = macos::collect().await;
        #[cfg(not(target_os = "macos"))]
        let result = other::collect().await;
        if let Some(devices) = result {
            self.previous = devices;
        } else {
            // Retain static device names across driver errors, never old load.
            for gpu in &mut self.previous {
                gpu.usage_percent = None;
                gpu.memory_used_bytes = None;
            }
        }
        self.previous.clone()
    }
}

#[cfg(any(test, not(target_os = "macos")))]
pub(super) fn nvidia_rows(raw: &str) -> Vec<GpuResources> {
    raw.lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split(',').map(str::trim).collect();
            if fields.len() != 4 || fields[0].is_empty() {
                return None;
            }
            let mib = |raw: &str| {
                raw.parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite() && *v >= 0.0 && *v <= u64::MAX as f64 / MIB_BYTES)
                    .map(|v| (v * MIB_BYTES) as u64)
            };
            Some(GpuResources {
                name: fields[0].into(),
                memory_total_bytes: mib(fields[1]),
                memory_used_bytes: mib(fields[2]),
                usage_percent: fields[3].parse().ok().and_then(super::valid_percent),
                shared_memory: false,
            })
        })
        .collect()
}
