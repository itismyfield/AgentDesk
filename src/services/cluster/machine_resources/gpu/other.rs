use super::{GpuResources, command, nvidia_rows};

pub(super) async fn collect() -> Option<Vec<GpuResources>> {
    if let Some(output) = command(
        "nvidia-smi",
        &[
            "--query-gpu=name,memory.total,memory.used,utilization.gpu",
            "--format=csv,noheader,nounits",
        ],
    )
    .await
    {
        let rows = nvidia_rows(std::str::from_utf8(&output).ok()?);
        if !rows.is_empty() {
            return Some(rows);
        }
    }
    fallback().await
}

#[cfg(windows)]
async fn fallback() -> Option<Vec<GpuResources>> {
    // WMI's 32-bit AdapterRAM overflows on modern GPUs. Keep unknown capacity
    // unknown when the driver does not provide the accurate NVIDIA counters.
    let output = command("powershell.exe", &["-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; @(Get-CimInstance Win32_VideoController -ErrorAction Stop | ForEach-Object { $_.Name }) | ConvertTo-Json -Compress",
    ]).await?;
    let names: serde_json::Value = serde_json::from_slice(&output).ok()?;
    let names: Vec<_> = names
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .or_else(|| names.as_str().map(|n| vec![n]))?;
    Some(
        names
            .into_iter()
            .map(|name| GpuResources {
                name: name.into(),
                usage_percent: None,
                memory_used_bytes: None,
                memory_total_bytes: None,
                shared_memory: false,
            })
            .collect(),
    )
}

#[cfg(not(windows))]
async fn fallback() -> Option<Vec<GpuResources>> {
    tokio::task::spawn_blocking(collect_drm)
        .await
        .ok()
        .flatten()
}

#[cfg(not(windows))]
fn collect_drm() -> Option<Vec<GpuResources>> {
    // AMD's DRM counters are unprivileged and report VRAM in bytes. Other
    // drivers may expose identification without a utilization counter.
    let rows = std::fs::read_dir("/sys/class/drm").ok()?;
    let mut gpus = Vec::new();
    for entry in rows.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name
            .strip_prefix("card")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let device = entry.path().join("device");
        let read = |file: &str| {
            std::fs::read_to_string(device.join(file))
                .ok()
                .map(|s| s.trim().to_owned())
        };
        let Some(vendor) = read("vendor") else {
            continue;
        };
        let brand = match vendor.as_str() {
            "0x1002" => "AMD",
            "0x8086" => "Intel",
            "0x10de" => "NVIDIA",
            _ => "GPU",
        };
        gpus.push(GpuResources {
            name: format!("{brand} {} ({name})", read("device").unwrap_or_default()),
            usage_percent: read("gpu_busy_percent")
                .and_then(|s| s.parse().ok())
                .and_then(super::super::valid_percent),
            memory_used_bytes: read("mem_info_vram_used").and_then(|s| s.parse().ok()),
            memory_total_bytes: read("mem_info_vram_total").and_then(|s| s.parse().ok()),
            shared_memory: false,
        });
    }
    Some(gpus)
}
