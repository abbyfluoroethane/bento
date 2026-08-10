// Small formatting helpers.

export function formatMiB(mib: number): string {
  if (mib >= 1024 && mib % 1024 === 0) return `${mib / 1024} GiB`;
  return `${mib} MiB`;
}

export function relativeTime(iso: string): string {
  if (!iso) return "never";
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return "never";
  const s = Math.max(0, Math.floor((Date.now() - then) / 1000));
  if (s < 60) return "just now";
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} min ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h} h ago`;
  const d = Math.floor(h / 24);
  if (d < 30) return `${d} d ago`;
  return new Date(then).toISOString().slice(0, 10);
}

export function shortChecksum(sum: string): string {
  if (!sum) return "—";
  return sum.length > 12 ? sum.slice(0, 12) + "…" : sum;
}

export function cooldownText(seconds: number): string {
  if (seconds >= 3600) return `${Math.ceil(seconds / 3600)} h`;
  if (seconds >= 60) return `${Math.ceil(seconds / 60)} min`;
  return `${seconds} s`;
}
