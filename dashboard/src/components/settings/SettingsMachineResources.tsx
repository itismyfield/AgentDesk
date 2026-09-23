import { Cpu, Gpu, HardDrive, MemoryStick } from "lucide-react";
import type { ReactNode } from "react";
import type { MachineResources } from "../../api/machineResources";
import type { SettingsTr } from "./SettingsPanelTypes";

// Presentation thresholds only; these colors do not control task admission.
const BUSY_PERCENT = 80;
const CRITICAL_PERCENT = 95;
const BYTES_PER_KIB = 1024;
const BYTE_UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
// Match the execution probe's tolerance for small peer clock differences.
const CLOCK_SKEW_TOLERANCE_MS = 5_000;

export function resourceBytes(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value) || value < 0) return "—";
  let unit = 0;
  while (value >= BYTES_PER_KIB && unit < BYTE_UNITS.length - 1) { value /= BYTES_PER_KIB; unit++; }
  return `${value.toLocaleString(undefined, { maximumFractionDigits: unit ? 1 : 0 })} ${BYTE_UNITS[unit]}`;
}

function percentage(used: number | null | undefined, total: number | null | undefined): number | null {
  return used != null && total != null && total > 0 ? Math.min(100, used / total * 100) : null;
}

function Meter({ value, label, tr }: { value: number | null; label: string; tr: SettingsTr }) {
  const color = value != null && value >= CRITICAL_PERCENT ? "bg-red-400"
    : value != null && value >= BUSY_PERCENT ? "bg-amber-400" : "bg-sky-400";
  return <div className="space-y-2">
    <div className="flex items-baseline justify-between gap-2 text-xs">
      <span className="text-th-text-muted">{label}</span>
      <strong className="text-base tabular-nums">{value == null ? tr("미확인", "Unknown") : `${value.toFixed(1)}%`}</strong>
    </div>
    <div className="h-1.5 overflow-hidden rounded-full bg-th-border" role={value == null ? undefined : "meter"} aria-label={label}
      aria-valuemin={value == null ? undefined : 0} aria-valuemax={value == null ? undefined : 100} aria-valuenow={value ?? undefined}
      aria-valuetext={value == null ? undefined : `${value.toFixed(1)}%`}>
      {value != null && <div className={`h-full rounded-full transition-[width] duration-300 ${color}`} style={{ width: `${value}%` }} />}
    </div>
  </div>;
}

function Resource({ icon, title, children }: { icon: ReactNode; title: string; children: ReactNode }) {
  return <div className="min-w-0 space-y-3 rounded-xl border border-th-border p-3">
    <h5 className="flex items-center gap-2 text-xs font-semibold text-th-text-muted">{icon}{title}</h5>
    {children}
  </div>;
}

export function SettingsMachineResources({ resources, stale, now, tr }: {
  resources: MachineResources | null | undefined; stale: boolean; now: number; tr: SettingsTr;
}) {
  if (!resources) return <p className="mt-4 rounded-xl border border-th-border p-3 text-sm text-th-text-muted">
    {tr("하드웨어 정보를 기다리는 중입니다.", "Waiting for hardware information.")}
  </p>;
  const current = !stale && resources.observed_at_ms <= now + CLOCK_SKEW_TOLERANCE_MS && resources.expires_at_ms > now;
  const { cpu, memory, disks, gpus } = resources;
  return <section className="mt-4 space-y-3" aria-label={tr("하드웨어 및 사용량", "Hardware and utilization")}>
    <div className="flex flex-wrap items-center justify-between gap-2 text-xs text-th-text-muted">
      <h4 className="font-semibold text-th-text">{tr("하드웨어 및 사용량", "Hardware and utilization")}</h4>
      <span>{current ? tr("측정", "Sampled") : tr("측정 정보 만료", "Sample expired")}: {new Date(resources.observed_at_ms).toLocaleTimeString(tr("ko-KR", "en-US"))}</span>
    </div>
    <div className="grid min-w-0 gap-3 sm:grid-cols-2">
      <Resource icon={<Cpu size={15} aria-hidden />} title="CPU">
        <p className="break-words text-sm font-medium">{cpu.model || tr("모델 미확인", "Model unknown")}</p>
        <p className="text-xs text-th-text-muted">{cpu.physical_cores == null ? "" : tr(`${cpu.physical_cores}코어 · `, `${cpu.physical_cores} cores · `)}{tr(`${cpu.logical_cores} 논리 프로세서`, `${cpu.logical_cores} logical processors`)}</p>
        <Meter value={current ? cpu.usage_percent : null} label={tr("CPU 사용률", "CPU utilization")} tr={tr} />
      </Resource>
      <Resource icon={<MemoryStick size={15} aria-hidden />} title={tr("메모리", "Memory")}>
        <p className="text-sm font-medium tabular-nums">{resourceBytes(current ? memory?.used_bytes : null)} / {resourceBytes(memory?.total_bytes)}</p>
        <p className="text-xs text-th-text-muted">{tr("사용 가능", "Available")}: {resourceBytes(current ? memory?.available_bytes : null)}</p>
        <Meter value={current ? percentage(memory?.used_bytes, memory?.total_bytes) : null} label={tr("메모리 사용률", "Memory utilization")} tr={tr} />
      </Resource>
      <Resource icon={<Gpu size={15} aria-hidden />} title="GPU">
        {gpus.length === 0 && <p className="text-xs text-th-text-muted">{tr("GPU 정보 미확인", "GPU information unavailable")}</p>}
        {gpus.map((gpu, index) => <div key={`${gpu.name}-${index}`} className="space-y-2">
          <p className="break-words text-sm font-medium">{gpu.name}</p>
          <Meter value={current ? gpu.usage_percent : null} label={tr("GPU 사용률", "GPU utilization")} tr={tr} />
          <p className="text-xs text-th-text-muted tabular-nums">{gpu.shared_memory ? tr("공유 메모리 사용", "Shared memory used") : "VRAM"}: {resourceBytes(current ? gpu.memory_used_bytes : null)}{!gpu.shared_memory && ` / ${resourceBytes(gpu.memory_total_bytes)}`}</p>
        </div>)}
      </Resource>
      <Resource icon={<HardDrive size={15} aria-hidden />} title={tr("디스크", "Disks")}>
        {disks.length === 0 && <p className="text-xs text-th-text-muted">{tr("디스크 정보 미확인", "Disk information unavailable")}</p>}
        {disks.map(disk => <div key={disk.mount_point} className="space-y-2">
          <p className="break-all text-xs font-medium">{disk.mount_point} <span className="text-th-text-muted">{disk.kind === "disk" ? disk.name : disk.kind}</span></p>
          <p className="text-xs tabular-nums">{resourceBytes(current ? disk.used_bytes : null)} / {resourceBytes(disk.total_bytes)}</p>
          <Meter value={current ? percentage(disk.used_bytes, disk.total_bytes) : null} label={tr("디스크 사용률", "Disk utilization")} tr={tr} />
        </div>)}
      </Resource>
    </div>
  </section>;
}
