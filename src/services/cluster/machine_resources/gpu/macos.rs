use super::GpuResources;

#[cfg(target_os = "macos")]
pub(super) async fn collect() -> Option<Vec<GpuResources>> {
    let output = super::command(
        "/usr/sbin/ioreg",
        &["-r", "-d", "1", "-c", "IOAccelerator", "-a"],
    )
    .await?;
    parse(&output)
}

fn parse(output: &[u8]) -> Option<Vec<GpuResources>> {
    let root = plist::Value::from_reader_xml(output).ok()?;
    let rows = root.as_array()?;
    Some(
        rows.iter()
            .filter_map(|row| {
                let row = row.as_dictionary()?;
                let name = row
                    .get("model")
                    .and_then(plist::Value::as_string)
                    .or_else(|| row.get("IOClass").and_then(plist::Value::as_string))?;
                let stats = row
                    .get("PerformanceStatistics")
                    .and_then(plist::Value::as_dictionary);
                let shared_memory = name.starts_with("Apple ")
                    || row
                        .get("IOClass")
                        .and_then(plist::Value::as_string)
                        .is_some_and(|class| class.starts_with("AGX"));
                let number = |key: &str| {
                    stats
                        .and_then(|s| s.get(key))
                        .and_then(plist::Value::as_unsigned_integer)
                };
                Some(GpuResources {
                    name: name.into(),
                    usage_percent: number("Device Utilization %")
                        .map(|v| v as f32)
                        .and_then(super::super::valid_percent),
                    memory_used_bytes: shared_memory
                        .then(|| number("In use system memory"))
                        .flatten(),
                    // Apple Silicon uses unified memory. No invented VRAM capacity.
                    memory_total_bytes: None,
                    shared_memory,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn discrete_gpu_does_not_present_system_allocations_as_vram() {
        let xml = br#"<?xml version="1.0"?><plist version="1.0"><array><dict>
            <key>model</key><string>Example discrete GPU</string><key>PerformanceStatistics</key><dict>
            <key>Device Utilization %</key><integer>21</integer>
            <key>In use system memory</key><integer>1048576</integer>
            </dict></dict></array></plist>"#;
        let gpus = super::parse(xml).unwrap();
        assert_eq!(gpus[0].usage_percent, Some(21.0));
        assert_eq!(gpus[0].memory_used_bytes, None);
        assert!(!gpus[0].shared_memory);
    }

    #[test]
    fn apple_gpu_preserves_shared_memory_and_missing_metrics() {
        let xml = br#"<?xml version="1.0"?><plist version="1.0"><array><dict>
            <key>model</key><string>Apple GPU</string><key>PerformanceStatistics</key><dict>
            <key>Device Utilization %</key><integer>37</integer>
            <key>In use system memory</key><integer>1048576</integer>
            </dict></dict></array></plist>"#;
        let gpus = super::parse(xml).unwrap();
        assert_eq!(gpus[0].usage_percent, Some(37.0));
        assert_eq!(gpus[0].memory_used_bytes, Some(1048576));
        assert!(gpus[0].shared_memory);
        assert_eq!(gpus[0].memory_total_bytes, None);
        assert!(super::parse(b"not plist").is_none());
    }
}
