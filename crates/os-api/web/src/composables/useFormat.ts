// =============================================================================
// useFormat —— 视图层格式化辅助（composable 风格）。
//
// Shares.vue / Users.vue / Nodes.vue / Settings.vue 移植自 static/js，引用
// protocolBadgeClass / roleBadgeClass / fmtCapacityGb / cpuVendorLabel /
// nestedVirtLabel。底层字节/时间格式化复用 @/utils/format，本文件只补徽章与
// CPU 虚拟化语义化标签（与 nodes.js / vms.js / settings.js 对齐）。
// =============================================================================

/** 协议 → 徽章 CSS 类（smb=info, nfs=ok, webdav=warn, 其它=muted）。 */
export function protocolBadgeClass(proto: string | null | undefined): string {
  const p = String(proto || '').toLowerCase();
  switch (p) {
    case 'smb':
      return 'badge-info';
    case 'nfs':
      return 'badge-ok';
    case 'webdav':
      return 'badge-warn';
    default:
      return 'badge-muted';
  }
}

/** 用户角色 → 徽章 CSS 类（admin=err, operator=info, viewer=muted）。 */
export function roleBadgeClass(role: string | null | undefined): string {
  const r = String(role || '').toLowerCase();
  switch (r) {
    case 'admin':
      return 'badge-err';
    case 'operator':
      return 'badge-info';
    case 'viewer':
    case 'guest':
      return 'badge-muted';
    default:
      return 'badge-info';
  }
}

/** GB 数字 → 文本（"1.0 TB" / "500 GB" / "—"）。null/非数字返回 '—'。 */
export function fmtCapacityGb(gb: number | null | undefined): string {
  if (gb == null || Number.isNaN(Number(gb))) return '—';
  const v = Number(gb);
  if (v < 0) return '—';
  if (v >= 1024) return (v / 1024).toFixed(v % 1024 === 0 ? 0 : 1) + ' TB';
  return `${Math.round(v)} GB`;
}

/** CPU 厂商 → 友好标签（Intel/AMD/ARM/未知）。
 *
 * 接受后端 `CpuVendor` 枚举的 serde 默认（externally-tagged）序列化形态：
 *   - `"Intel"`           ← CpuVendor::Intel
 *   - `"Amd"`             ← CpuVendor::Amd
 *   - `{"Unknown": "x"}`  ← CpuVendor::Unknown(String)
 * 也兼容旧的纯字符串输入（"Intel"/"AMD"/"GenuineIntel"…）。
 */
export function cpuVendorLabel(vendor: unknown): string {
  if (vendor == null) return '未知';
  // serde 标签枚举：CpuVendor::Unknown(String) → { "Unknown": "<原始 vendor_id>" }
  if (typeof vendor === 'object') {
    const obj = vendor as Record<string, unknown>;
    // 取任一枚举变体携带的内嵌字符串值（典型为 Unknown("GenuineCPU")）
    const inner =
      (obj.Unknown as string | undefined) ??
      (obj.unknown as string | undefined) ??
      (obj.Intel as string | undefined) ??
      (obj.Amd as string | undefined);
    if (inner != null) return cpuVendorLabel(inner);
    return '未知';
  }
  const v = String(vendor).toLowerCase();
  if (!v) return '未知';
  if (v.includes('genuineintel') || v === 'intel') return 'Intel';
  if (v.includes('authenticamd') || v === 'amd') return 'AMD';
  if (v.includes('arm')) return 'ARM';
  // 兜底：返回原始字符串（绝不能显示 [object Object]）
  return String(vendor);
}

/** 嵌套虚拟化 → 友好标签（✓ 已启用 / ✗ 未启用 / — 未知）。
 *
 * 接受后端 `NestedVirtStatus` 枚举的 serde 默认序列化形态：
 *   - `{"Supported": true}`  ← NestedVirtStatus::Supported(true)
 *   - `{"Supported": false}` ← NestedVirtStatus::Supported(false)
 *   - `"Unknown"`            ← NestedVirtStatus::Unknown
 * 也兼容旧的纯布尔 / 'true' 字符串输入。
 */
export function nestedVirtLabel(v: unknown): string {
  if (v == null) return '—';
  // serde 标签枚举：Supported(bool) → { "Supported": true|false }
  if (typeof v === 'object') {
    const obj = v as Record<string, unknown>;
    if ('Supported' in obj || 'supported' in obj) {
      const enabled = obj.Supported ?? obj.supported;
      if (enabled === true) return '✓ 已启用';
      if (enabled === false) return '✗ 未启用';
    }
    // "Unknown" 不会被外层覆盖为对象，但稳妥起见也兼容大小写键
    if ('Unknown' in obj || 'unknown' in obj) return '—';
    return '—';
  }
  // 标量 / 字符串变体（"Unknown" / true / 'true' / 1 …）
  const s = String(v).toLowerCase();
  if (s === 'true' || s === '1') return '✓ 已启用';
  if (s === 'false' || s === '0') return '✗ 未启用';
  // "Unknown" 或其它 → 未知
  return '—';
}
