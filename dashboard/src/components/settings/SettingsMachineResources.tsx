import { Cpu, Gpu, HardDrive, MemoryStick, Network } from "lucide-react";
import type { ReactNode } from "react";
import type { MachineResources } from "../../api/machineResources";
import type { SettingsTr } from "./SettingsPanelTypes";
import { MachineSparkline, metricColors, readings, recentResources, type TrendColor } from "./MachineSparkline";

const BYTES_PER_KIB = 1024;
const BYTE_UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
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
  return <div className="text-sm tabular-nums" role={value == null ? undefined : "meter"} aria-label={label}
    aria-valuemin={value == null ? undefined : 0} aria-valuemax={value == null ? undefined : 100}
    aria-valuenow={value ?? undefined} aria-valuetext={value == null ? undefined : `${value.toFixed(1)}%`}>
    <span className="sr-only">{label}: </span>{value == null ? tr("미확인", "Unknown") : `${value.toFixed(1)}%`}
  </div>;
}

function Resource({ icon, title, color, trend, children }: {
  icon: ReactNode; title: string; color: TrendColor; trend: ReactNode; children: ReactNode;
}) {
  return <div className="grid min-w-0 grid-cols-[88px_minmax(0,1fr)] items-center gap-3 py-3 sm:grid-cols-[104px_minmax(0,1fr)]"
    data-hardware-kind={color}>
    <div className="min-w-0">{trend}</div>
    <div className="min-w-0 space-y-1">
      <h5 className="flex items-center gap-2 text-sm font-semibold"><span style={{ color: metricColors[color] }}>{icon}</span>{title}</h5>
      {children}
    </div>
  </div>;
}

export function SettingsMachineResources({ resources, history = [], stale, now, tr }: {
  resources: MachineResources | null | undefined; history?: MachineResources[]; stale: boolean; now: number; tr: SettingsTr;
}) {
  if (!resources) return <p className="mt-4 rounded-xl border border-th-border p-3 text-sm text-th-text-muted">
    {tr("하드웨어 정보를 기다리는 중입니다.", "Waiting for hardware information.")}
  </p>;
  const current = !stale && resources.observed_at_ms <= now + CLOCK_SKEW_TOLERANCE_MS && resources.expires_at_ms > now;
  const { cpu, memory, disks, gpus, network } = resources;
  const samples = recentResources(history, resources, now);
  const trend = (color: TrendColor, label: string, select: (sample: MachineResources) => number | null | undefined,
    secondary?: (sample: MachineResources) => number | null | undefined) => <MachineSparkline
    values={readings(samples, select)} secondary={secondary && readings(samples, secondary)}
    color={color} label={label} stale={!current} now={now} tr={tr} />;
  return <section className="mt-4 rounded-lg bg-[#202020] p-3 text-neutral-100" aria-label={tr("하드웨어 및 사용량", "Hardware and utilization")}>
    <div className="mb-1 flex flex-wrap items-center justify-between gap-2 text-xs text-neutral-400">
      <h4 className="font-semibold text-neutral-100">{tr("하드웨어 및 사용량", "Hardware and utilization")}</h4>
      <span>{tr("최근 15분 추이", "Last 15 minutes")}</span>
    </div>
    <p className="text-[11px] text-neutral-400">{current ? tr("측정", "Sampled") : tr("측정 정보 만료", "Sample expired")}: {new Date(resources.observed_at_ms).toLocaleTimeString(tr("ko-KR", "en-US"))}</p>
    <div className="divide-y divide-neutral-700">
      <Resource icon={<Cpu size={15} aria-hidden />} title="CPU" color="cpu"
        trend={trend("cpu", "CPU", sample => sample.cpu.usage_percent)}>
        <p className="break-words text-xs text-neutral-400">{cpu.model || tr("모델 미확인", "Model unknown")}</p>
        <p className="text-[11px] text-neutral-400">{cpu.physical_cores == null ? "" : tr(`${cpu.physical_cores}코어 · `, `${cpu.physical_cores} cores · `)}{tr(`${cpu.logical_cores} 논리 프로세서`, `${cpu.logical_cores} logical processors`)}</p>
        <Meter value={current ? cpu.usage_percent : null} label={tr("CPU 사용률", "CPU utilization")} tr={tr} />
      </Resource>
      <Resource icon={<MemoryStick size={15} aria-hidden />} title={tr("메모리", "Memory")} color="memory"
        trend={trend("memory", tr("메모리", "Memory"), sample => percentage(sample.memory?.used_bytes, sample.memory?.total_bytes))}>
        <p className="text-sm tabular-nums">{resourceBytes(current ? memory?.used_bytes : null)} / {resourceBytes(memory?.total_bytes)}</p>
        <Meter value={current ? percentage(memory?.used_bytes, memory?.total_bytes) : null} label={tr("메모리 사용률", "Memory utilization")} tr={tr} />
        <p className="text-[11px] text-neutral-400">{tr("사용 가능", "Available")}: {resourceBytes(current ? memory?.available_bytes : null)}</p>
      </Resource>
      {disks.length === 0 && <p className="py-3 text-xs text-neutral-400">{tr("디스크 정보 미확인", "Disk information unavailable")}</p>}
      {disks.map((disk, index) => <Resource key={disk.mount_point} icon={<HardDrive size={15} aria-hidden />}
        title={`${tr("디스크", "Disk")} ${index} (${disk.mount_point})`} color="disk"
        trend={trend("disk", disk.mount_point, sample => {
          const prior = sample.disks.find(item => item.mount_point === disk.mount_point);
          return percentage(prior?.used_bytes, prior?.total_bytes);
        })}>
        <p className="break-words text-xs text-neutral-400">{disk.kind === "disk" ? disk.name : disk.kind}</p>
        <p className="text-sm tabular-nums">{resourceBytes(current ? disk.used_bytes : null)} / {resourceBytes(disk.total_bytes)}</p>
        <Meter value={current ? percentage(disk.used_bytes, disk.total_bytes) : null} label={tr("디스크 공간 사용률", "Disk space utilization")} tr={tr} />
      </Resource>)}
      <Resource icon={<Network size={15} aria-hidden />} title={network?.wired ? tr("이더넷", "Ethernet") : tr("네트워크", "Network")} color="network"
        trend={trend("network", network?.interface ?? tr("네트워크", "Network"),
          sample => sample.network?.interface === network?.interface ? sample.network?.received_bytes_per_sec : null,
          sample => sample.network?.interface === network?.interface ? sample.network?.transmitted_bytes_per_sec : null)}>
        {network ? <>
          <p className="break-words text-xs text-neutral-400">{network.interface}</p>
          <p className="text-xs tabular-nums"><span style={{ color: metricColors.network }}>●</span> {tr("받기", "Receive")}: {resourceBytes(current ? network.received_bytes_per_sec : null)}/s</p>
          <p className="text-xs tabular-nums"><span style={{ color: metricColors.networkOut }}>●</span> {tr("보내기", "Send")}: {resourceBytes(current ? network.transmitted_bytes_per_sec : null)}/s</p>
        </> : <p className="text-xs text-neutral-400">{tr("활성 네트워크를 확인하는 중입니다.", "Waiting for active network data.")}</p>}
      </Resource>
      {gpus.length === 0 && <p className="py-3 text-xs text-neutral-400">{tr("GPU 정보 미확인", "GPU information unavailable")}</p>}
      {gpus.map((gpu, index) => {
        const ordinal = gpus.slice(0, index).filter(item => item.name === gpu.name).length;
        return <Resource key={`${gpu.name}-${ordinal}`} icon={<Gpu size={15} aria-hidden />} title={`GPU ${index}`} color="gpu"
          trend={trend("gpu", gpu.name, sample => sample.gpus.filter(item => item.name === gpu.name)[ordinal]?.usage_percent)}>
          <p className="break-words text-xs text-neutral-400">{gpu.name}</p>
          <Meter value={current ? gpu.usage_percent : null} label={tr("GPU 사용률", "GPU utilization")} tr={tr} />
          <p className="text-xs text-neutral-400 tabular-nums">{gpu.shared_memory ? tr("공유 메모리 사용", "Shared memory used") : "VRAM"}: {resourceBytes(current ? gpu.memory_used_bytes : null)}{!gpu.shared_memory && ` / ${resourceBytes(gpu.memory_total_bytes)}`}</p>
        </Resource>;
      })}
    </div>
  </section>;
}
