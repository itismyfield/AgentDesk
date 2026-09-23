import { z } from "zod";

const bytes = z.number().finite().nonnegative();
const percent = z.number().finite().min(0).max(100).nullable();

export const machineResourcesSchema = z.object({
  schema: z.literal(1),
  observed_at_ms: z.number().finite(), expires_at_ms: z.number().finite(),
  sample_interval_ms: z.number().positive(),
  cpu: z.object({
    model: z.string(), physical_cores: z.number().int().positive().nullable(),
    logical_cores: z.number().int().nonnegative(), usage_percent: percent,
  }),
  memory: z.object({ total_bytes: bytes, used_bytes: bytes, available_bytes: bytes }).nullable(),
  disks: z.array(z.object({
    name: z.string(), mount_point: z.string(), kind: z.string(),
    total_bytes: bytes, used_bytes: bytes, available_bytes: bytes,
  })),
  gpus: z.array(z.object({
    name: z.string(), usage_percent: percent,
    memory_used_bytes: bytes.nullable(), memory_total_bytes: bytes.nullable(), shared_memory: z.boolean(),
  })),
});

export type MachineResources = z.infer<typeof machineResourcesSchema>;
