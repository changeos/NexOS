// =============================================================================
// icon.ts —— 二维码传输图标（QR 码方块定位图）。
//
// SVG 内部标记（viewBox 0 0 24 24 / stroke currentColor 体系，与主前端 AppIcon
// 包装层一致；原 AppIcon ICONS.qrtransfer 条目随应用剥离迁入）。
// registerApp 时经 ctx.registerApp({ icon }) 交给宿主注册到 runtimeIcons，
// 桌面 / Dock / 启动台统一渲染。
// =============================================================================
export const QRTRANSFER_ICON =
  '<rect x="3" y="3" width="7" height="7" rx="1"/>' +
  '<rect x="14" y="3" width="7" height="7" rx="1"/>' +
  '<rect x="3" y="14" width="7" height="7" rx="1"/>' +
  '<rect x="5" y="5" width="3" height="3" fill="currentColor" stroke="none"/>' +
  '<rect x="16" y="5" width="3" height="3" fill="currentColor" stroke="none"/>' +
  '<rect x="5" y="16" width="3" height="3" fill="currentColor" stroke="none"/>' +
  '<rect x="14" y="14" width="2.5" height="2.5" fill="currentColor" stroke="none"/>' +
  '<rect x="18.5" y="14" width="2.5" height="2.5" fill="currentColor" stroke="none"/>' +
  '<rect x="14" y="18.5" width="2.5" height="2.5" fill="currentColor" stroke="none"/>' +
  '<rect x="18.5" y="18.5" width="2.5" height="2.5" fill="currentColor" stroke="none"/>' +
  '<path d="M16.5 16.5h2" opacity="0.7"/><path d="M19.75 16.5v0" opacity="0.7"/>'
