// =============================================================================
// icon.ts —— 流媒体中心图标（信号塔/广播波纹）。
//
// SVG 内部标记（viewBox 0 0 24 24 / stroke currentColor 体系，与主前端 AppIcon
// 包装层一致；原 AppIcon ICONS.streaming 条目随应用剥离迁入）。
// registerApp 时经 ctx.registerApp({ icon }) 交给宿主注册到 runtimeIcons，
// 桌面 / Dock / 启动台统一渲染。
// =============================================================================
export const STREAMING_ICON =
  '<path d="M5 20l5-12"/>' +
  '<path d="M19 20l-5-12"/>' +
  '<path d="M8 14h8"/>' +
  '<path d="M12 8V4"/>' +
  '<circle cx="12" cy="3" r="1.4" fill="currentColor" stroke="none"/>' +
  '<path d="M9.5 6.5a3.5 3.5 0 0 0-2 3"/>' +
  '<path d="M14.5 6.5a3.5 3.5 0 0 1 2 3"/>' +
  '<path d="M7.5 5a6.5 6.5 0 0 0-3 5"/>' +
  '<path d="M16.5 5a6.5 6.5 0 0 1 3 5"/>'
