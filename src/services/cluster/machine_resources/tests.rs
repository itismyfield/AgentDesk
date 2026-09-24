use super::*;

#[test]
fn first_cpu_sample_is_unknown_and_memory_and_disk_accounting_are_bounded() {
    let snapshot = sampler::Sampler::new(None, Default::default()).collect(MIN_SAMPLE_TTL);
    assert_eq!(snapshot.cpu.usage_percent, None);
    assert_eq!(snapshot.expires_at_ms - snapshot.observed_at_ms, 30_000);
    if let Some(memory) = snapshot.memory {
        assert_eq!(
            memory.used_bytes + memory.available_bytes,
            memory.total_bytes
        );
    }
    for disk in snapshot.disks {
        assert_eq!(disk.used_bytes + disk.available_bytes, disk.total_bytes);
    }
}

#[test]
fn non_finite_usage_is_not_a_healthy_zero() {
    assert_eq!(valid_percent(f32::NAN), None);
    assert_eq!(valid_percent(f32::INFINITY), None);
    assert_eq!(valid_percent(-1.0), None);
    assert_eq!(valid_percent(102.0), Some(100.0));
}

#[test]
fn nvidia_counters_preserve_large_vram_and_unavailable_values() {
    let rows =
        gpu::nvidia_rows("Example GPU, 16384, 8192, 42\nOther GPU, [N/A], [N/A], [N/A]\nmalformed");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].memory_total_bytes, Some(16 * 1024 * 1024 * 1024));
    assert_eq!(rows[0].memory_used_bytes, Some(8 * 1024 * 1024 * 1024));
    assert_eq!(rows[0].usage_percent, Some(42.0));
    assert_eq!(rows[1].usage_percent, None);
    assert_eq!(rows[1].memory_total_bytes, None);
}
