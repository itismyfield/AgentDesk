use std::time::Duration;

use sysinfo::{CpuRefreshKind, DiskKind, Disks, MemoryRefreshKind, RefreshKind, System};

use super::{CpuResources, DiskResources, MachineResources, MemoryResources, SAMPLE_INTERVAL};

pub(super) struct Sampler {
    system: System,
    disks: Disks,
    physical_cores: Option<usize>,
    primed: bool,
}

impl Sampler {
    pub(super) fn new() -> Self {
        Self {
            system: System::new_with_specifics(
                RefreshKind::nothing().with_cpu(CpuRefreshKind::nothing().with_cpu_usage()),
            ),
            disks: Disks::new(),
            physical_cores: System::physical_core_count(),
            primed: false,
        }
    }

    pub(super) fn collect(&mut self, ttl: Duration) -> MachineResources {
        self.system.refresh_cpu_usage();
        self.system
            .refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        self.disks.refresh(true);
        let observed_at_ms = chrono::Utc::now().timestamp_millis();
        let cpu = CpuResources {
            model: self
                .system
                .cpus()
                .first()
                .map(|c| c.brand().trim())
                .unwrap_or("")
                .into(),
            physical_cores: self.physical_cores,
            logical_cores: self.system.cpus().len(),
            // A CPU percentage requires two distinct samples. Do not present
            // the first refresh's placeholder value as a measured idle host.
            usage_percent: (self.primed && !self.system.cpus().is_empty())
                .then(|| self.system.global_cpu_usage())
                .and_then(super::valid_percent),
        };
        self.primed = true;
        let total = self.system.total_memory();
        let available = self.system.available_memory().min(total);
        let memory = (total > 0).then_some(MemoryResources {
            total_bytes: total,
            used_bytes: total - available,
            available_bytes: available,
        });
        let mut disks: Vec<_> = self
            .disks
            .iter()
            .filter(|d| d.total_space() > 0)
            .filter(|d| visible_mount(d.mount_point().to_string_lossy().as_ref()))
            .map(|disk| {
                let total = disk.total_space();
                let available = disk.available_space().min(total);
                DiskResources {
                    name: disk.name().to_string_lossy().into_owned(),
                    mount_point: disk.mount_point().to_string_lossy().into_owned(),
                    kind: match disk.kind() {
                        DiskKind::SSD => "SSD",
                        DiskKind::HDD => "HDD",
                        _ => "disk",
                    }
                    .into(),
                    total_bytes: total,
                    used_bytes: total - available,
                    available_bytes: available,
                }
            })
            .collect();
        disks.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
        MachineResources {
            schema: 1,
            observed_at_ms,
            expires_at_ms: observed_at_ms
                .saturating_add(ttl.as_millis().min(i64::MAX as u128) as i64),
            sample_interval_ms: SAMPLE_INTERVAL.as_millis() as u64,
            cpu,
            memory,
            disks,
            gpus: Vec::new(),
        }
    }
}

fn visible_mount(mount: &str) -> bool {
    // APFS system support volumes share the root container. Showing them as
    // independent disks multiplies the same capacity and obscures real drives.
    !cfg!(target_os = "macos") || !mount.starts_with("/System/Volumes/")
}
