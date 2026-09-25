// =============================================================================
// 通用格式化工具（字节 / 时间 / uptime / 比率，移植自 dashboard.js / storage.js）
// =============================================================================

/** 字节 → 人类可读（二进制，1024）。null/NaN 返回 '—'。 */
export function formatBytes(n: number | null | undefined): string {
  if (n == null || Number.isNaN(Number(n))) return '—';
  const v = Number(n);
  if (v < 0) return '—';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
  let val = v;
  let i = 0;
  while (val >= 1024 && i < units.length - 1) {
    val /= 1024;
    i++;
  }
  return `${val.toFixed(val < 10 && i > 0 ? 1 : 0)} ${units[i]}`;
}

/** 秒 → 可读时长（如 1d 3h 20m）。null/NaN/负数 返回 '—'。 */
export function formatUptime(s: number | null | undefined): string {
  if (s == null || Number.isNaN(Number(s)) || Number(s) < 0) return '—';
  const total = Math.floor(Number(s));
  const d = Math.floor(total / 86400);
  const h = Math.floor((total % 86400) / 3600);
  const m = Math.floor((total % 3600) / 60);
  if (d > 0) return `${d}d ${h}h ${m}m`;
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m`;
}

/** MB → GiB/MiB 文本（VM 内存，移植自 vms.js fmtMem）。 */
export function formatMemoryMB(mb: number | null | undefined): string {
  const n = Number(mb);
  if (!n) return '—';
  if (n >= 1024) return (n / 1024).toFixed(n % 1024 === 0 ? 0 : 1) + ' GiB';
  return `${n} MiB`;
}

/**
 * 把 RFC3339 / ISO8601 字符串格式化为本地可读时间（YYYY-MM-DD HH:mm:ss）。
 * 解析失败原样返回。
 */
export function formatDateTime(iso: string | null | undefined): string {
  if (!iso) return '—';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const pad = (n: number) => (n < 10 ? `0${n}` : String(n));
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
    `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
  );
}

/** 比率 0~1 → 百分比整数。 */
export function ratioPct(r: number): number {
  const clamped = Math.max(0, Math.min(1, Number(r) || 0));
  return Math.round(clamped * 100);
}
