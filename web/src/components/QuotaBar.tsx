import type { Quota, Usage } from "../api";
import { formatMiB } from "../format";
import { cn } from "./ui/cn";

// The quota display above the instance table (SPEC 14.4): used amount and
// limit for all four limits of SPEC 6.1. The numbers are always written
// out — the meter color alone never carries the information.
export function QuotaBar({ quota, usage }: { quota: Quota | null; usage: Usage }) {
  const items = [
    { label: "Instances", used: usage.instances, max: quota?.max_instances, fmt: String },
    { label: "vCPU", used: usage.vcpu, max: quota?.max_vcpu, fmt: String },
    { label: "Memory", used: usage.memory_mib, max: quota?.max_memory_mib, fmt: formatMiB },
    { label: "Disk", used: usage.disk_gib, max: quota?.max_disk_gib, fmt: (n: number) => `${n} GiB` },
  ];
  return (
    <dl className="grid grid-cols-2 gap-3 sm:grid-cols-4">
      {items.map((it) => {
        const ratio = it.max ? Math.min(1, it.used / it.max) : 0;
        return (
          <div key={it.label} className="rounded-lg border border-surface1 bg-mantle p-3">
            <dt className="text-xs text-subtext0">{it.label}</dt>
            <dd className="mt-1 font-mono text-sm">
              {it.fmt(it.used)}
              <span className="text-subtext0">
                {" / "}
                {it.max !== undefined ? it.fmt(it.max) : "no limit"}
              </span>
            </dd>
            {it.max !== undefined && (
              <div aria-hidden className="mt-2 h-1.5 overflow-hidden rounded-full bg-surface0">
                <div
                  className={cn("h-full rounded-full", ratio >= 1 ? "bg-red" : "bg-accent")}
                  style={{ width: `${ratio * 100}%` }}
                />
              </div>
            )}
          </div>
        );
      })}
    </dl>
  );
}
