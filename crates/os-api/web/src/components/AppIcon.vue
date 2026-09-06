<script setup lang="ts">
/**
 * Ubuntu Yaru 风线条图标组件（复用 static/js/icons.js 的 SVG 路径）。
 * 所有图标 viewBox="0 0 24 24"，fill="none"，stroke="currentColor"，
 * stroke-width="1.5"，继承 CSS color。
 */
import { computed } from 'vue'
import { runtimeIcons } from '@/appRegistry'

const props = withDefaults(
    defineProps<{
        name: string
        size?: number | string
        /** 名字未命中 ICONS / runtimeIcons 时的兜底（SVG 内部标记；缺省空串 = 维持原有空渲染）。 */
        fallback?: string
    }>(),
    { size: 20, fallback: '' },
)

// 图标路径库（迁移自 crates/os-api/static/js/icons.js 的 ICONS）
const ICONS: Record<string, string> = {
    dashboard:
        '<rect x="3" y="3" width="7" height="7" rx="1"/>' +
        '<rect x="14" y="3" width="7" height="7" rx="1"/>' +
        '<rect x="3" y="14" width="7" height="7" rx="1"/>' +
        '<rect x="14" y="14" width="7" height="7" rx="1"/>',
    storage:
        '<ellipse cx="12" cy="5" rx="8" ry="2.5"/>' +
        '<path d="M4 5v6c0 1.38 3.58 2.5 8 2.5s8-1.12 8-2.5V5"/>' +
        '<path d="M4 11v6c0 1.38 3.58 2.5 8 2.5s8-1.12 8-2.5v-6"/>',
    vm:
        '<rect x="2.5" y="4" width="19" height="13" rx="2"/>' +
        '<path d="M8 21h8"/><path d="M12 17v4"/>' +
        '<path d="M7 9l-2 2 2 2"/><path d="M17 9l2 2-2 2"/>' +
        '<path d="M13.5 8l-3 6"/>',
    share:
        '<path d="M3 7a1 1 0 0 1 1-1h5l2 2h8a1 1 0 0 1 1 1v9a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1z"/>' +
        '<circle cx="16.5" cy="11.5" r="1.8"/>' +
        '<path d="M18 11.5V8.5a2 2 0 0 0-2-2h-2"/>',
    users:
        '<circle cx="9" cy="8" r="3.2"/>' +
        '<path d="M3.5 20a5.5 5.5 0 0 1 11 0"/>' +
        '<path d="M16 5.2a3 3 0 0 1 0 5.6"/>' +
        '<path d="M17.5 14.2a5.5 5.5 0 0 1 3 5.8"/>',
    nodes:
        '<circle cx="6" cy="6" r="2.2"/><circle cx="18" cy="6" r="2.2"/>' +
        '<circle cx="12" cy="18" r="2.2"/>' +
        '<path d="M7.6 7.6l3.2 8.4"/><path d="M16.4 7.6l-3.2 8.4"/>' +
        '<path d="M8.2 6h7.6"/>',
    settings:
        '<circle cx="12" cy="12" r="3"/>' +
        '<path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>',
    chat:
        '<path d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8v.5z"/>' +
        '<circle cx="9" cy="11.5" r="0.7" fill="currentColor" stroke="none"/>' +
        '<circle cx="12" cy="11.5" r="0.7" fill="currentColor" stroke="none"/>' +
        '<circle cx="15" cy="11.5" r="0.7" fill="currentColor" stroke="none"/>',
    modelchat:
        '<path d="M4 5h16a1 1 0 0 1 1 1v9a1 1 0 0 1-1 1H9l-4 3.5V16H4a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1z"/>' +
        '<circle cx="8.5" cy="10.5" r="0.8" fill="currentColor" stroke="none"/>' +
        '<circle cx="12" cy="10.5" r="0.8" fill="currentColor" stroke="none"/>' +
        '<circle cx="15.5" cy="10.5" r="0.8" fill="currentColor" stroke="none"/>' +
        '<path d="M18.5 3.2a4 4 0 0 1 2.3 3.6" opacity="0.6"/>' +
        '<path d="M19 1.4a6 6 0 0 1 3.4 5.4" opacity="0.4"/>',
    logo:
        '<rect x="3" y="2.5" width="18" height="19" rx="1.5"/>' +
        '<path d="M3 7h18"/><path d="M3 12h18"/><path d="M3 17h18"/>' +
        '<circle cx="6.5" cy="4.75" r="0.6" fill="currentColor" stroke="none"/>' +
        '<circle cx="6.5" cy="9.75" r="0.6" fill="currentColor" stroke="none"/>' +
        '<circle cx="6.5" cy="14.75" r="0.6" fill="currentColor" stroke="none"/>' +
        '<path d="M11 4.75h6"/><path d="M11 9.75h6"/><path d="M11 14.75h6"/>',
    // —— 补全桌面应用图标（与 DashboardView 内联 SVG 一致，便于 Dock/状态栏复用）——
    network:
        '<rect x="2.5" y="3" width="7" height="5.5" rx="1"/>' +
        '<rect x="14.5" y="3" width="7" height="5.5" rx="1"/>' +
        '<rect x="8.5" y="15.5" width="7" height="5.5" rx="1"/>' +
        '<path d="M6 8.5v3.5a1 1 0 0 0 1 1h4.5"/>' +
        '<path d="M18 8.5v3.5a1 1 0 0 1-1 1h-4.5"/>' +
        '<path d="M12 13v2.5"/>',
    provisioning:
        '<path d="M12 3c2.5 1.8 4 4.5 4 8v3h-8v-3c0-3.5 1.5-6.2 4-8z"/>' +
        '<circle cx="12" cy="9" r="1.6"/>' +
        '<path d="M8 14H5.5a1 1 0 0 0-1 1.2l1 4.8h13l1-4.8a1 1 0 0 0-1-1.2H16"/>' +
        '<path d="M9 20v1.5a1 1 0 0 0 1 1h4a1 1 0 0 0 1-1V20"/>' +
        '<path d="M12 14v2.5"/>',
    update:
        // 更新：环形双箭头（循环升级）+ 中心 A/B 槽位点
        '<path d="M4.5 12a7.5 7.5 0 0 1 13-5.1"/>' +
        '<path d="M17.5 4v3.2h-3.2"/>' +
        '<path d="M19.5 12a7.5 7.5 0 0 1-13 5.1"/>' +
        '<path d="M6.5 20v-3.2h3.2"/>' +
        '<circle cx="12" cy="12" r="2.2"/>' +
        '<path d="M12 9.8V7.5" opacity="0.7"/>' +
        '<path d="M12 16.5v-2.3" opacity="0.7"/>',
    backup:
        '<path d="M12 2.5l7.5 2.8v5.2c0 4.6-3.2 8.4-7.5 9.5-4.3-1.1-7.5-4.9-7.5-9.5V5.3L12 2.5z"/>' +
        '<circle cx="12" cy="10.5" r="3.4"/>' +
        '<path d="M12 8.8v1.9l1.3 1.3"/>',
    monitor:
        '<path d="M3 20h18"/><path d="M5 20a7 7 0 0 1 14 0"/>' +
        '<path d="M12 13v7"/><path d="M12 13l-2.5-3"/><path d="M12 13l2.5-3"/>' +
        '<path d="M6.5 11.5a6 6 0 0 1 11 0"/>',
    files:
        '<path d="M3 7a1 1 0 0 1 1-1h5l2 2h8a1 1 0 0 1 1 1v9a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1z"/>' +
        '<path d="M3 11h18"/>',
    downloads:
        '<path d="M12 3v10"/><path d="M8 10l4 4 4-4"/>' +
        '<path d="M4 17v2a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-2"/>',
    containers:
        '<rect x="8.5" y="2.5" width="7" height="5" rx="1"/>' +
        '<rect x="3" y="9.5" width="7" height="5" rx="1"/>' +
        '<rect x="14" y="9.5" width="7" height="5" rx="1"/>' +
        '<path d="M12 14.5V18"/><path d="M7 21h10"/>',
    surveillance:
        '<rect x="2.5" y="5.5" width="13" height="9" rx="2"/>' +
        '<path d="M15.5 8.5l5-2.5v10l-5-2.5"/>' +
        '<path d="M6 17.5v1.5a1 1 0 0 0 1 1h4"/>' +
        '<circle cx="6.5" cy="10" r="1.8"/>',
    cloudsync:
        '<path d="M6.5 18h10a4 4 0 0 0 .5-7.97 5.5 5.5 0 0 0-10.7-.8A3.75 3.75 0 0 0 6.5 18z"/>' +
        '<path d="M12 11v6"/><path d="M10 13l2-2 2 2"/>',
    notes:
        '<path d="M5 3h9l5 5v12a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1z"/>' +
        '<path d="M14 3v5h5"/>' +
        '<path d="M8 13l7-2.5"/><path d="M14.5 8.5l2 .7-4.5 9-2-.7z"/>',
    // 流媒体中心图标已随应用剥离（apps/streaming）：由应用包注册（runtimeIcons）
    llm:
        '<circle cx="5" cy="6" r="1.6" fill="currentColor" stroke="none"/>' +
        '<circle cx="5" cy="18" r="1.6" fill="currentColor" stroke="none"/>' +
        '<circle cx="12" cy="3" r="1.6" fill="currentColor" stroke="none"/>' +
        '<circle cx="12" cy="12" r="2" fill="currentColor" stroke="none"/>' +
        '<circle cx="12" cy="21" r="1.6" fill="currentColor" stroke="none"/>' +
        '<circle cx="19" cy="6" r="1.6" fill="currentColor" stroke="none"/>' +
        '<circle cx="19" cy="18" r="1.6" fill="currentColor" stroke="none"/>' +
        '<path d="M6.3 6.5L10.4 11"/><path d="M6.3 17.5L10.4 13"/>' +
        '<path d="M12 5v5"/><path d="M12 14v5"/>' +
        '<path d="M17.7 6.5L13.6 11"/><path d="M17.7 17.5L13.6 13"/>' +
        '<path d="M10 12h4"/>',
    gateway:
        '<circle cx="12" cy="12" r="2.4" fill="currentColor" stroke="none"/>' +
        '<path d="M12 9.6V6"/><path d="M12 14.4V18"/>' +
        '<path d="M9.6 12H6"/><path d="M14.4 12H18"/>' +
        '<circle cx="12" cy="4" r="1.4" fill="currentColor" stroke="none"/>' +
        '<circle cx="12" cy="20" r="1.4" fill="currentColor" stroke="none"/>' +
        '<circle cx="4" cy="12" r="1.4" fill="currentColor" stroke="none"/>' +
        '<circle cx="20" cy="12" r="1.4" fill="currentColor" stroke="none"/>' +
        '<path d="M5.2 7L9.5 9.5" opacity="0.6"/><path d="M18.8 7L14.5 9.5" opacity="0.6"/>' +
        '<path d="M5.2 17L9.5 14.5" opacity="0.6"/><path d="M18.8 17L14.5 14.5" opacity="0.6"/>',
    blockchain:
        '<rect x="3" y="9" width="6" height="6" rx="1"/><rect x="15" y="9" width="6" height="6" rx="1"/>' +
        '<rect x="9" y="3" width="6" height="6" rx="1"/><rect x="9" y="15" width="6" height="6" rx="1"/>' +
        '<path d="M9 6H7.5" opacity="0.6"/><path d="M15 6h1.5" opacity="0.6"/>' +
        '<path d="M9 18H7.5" opacity="0.6"/><path d="M15 18h1.5" opacity="0.6"/>',
    video:
        '<rect x="2.5" y="5" width="14" height="14" rx="2"/><path d="M16.5 9.5l5-3v11l-5-3z"/>',
    music:
        '<path d="M9 18V6l10-2v12"/>' +
        '<circle cx="6" cy="18" r="3"/><circle cx="16" cy="16" r="3"/>',
    photo:
        '<rect x="2.5" y="4" width="19" height="16" rx="2"/>' +
        '<circle cx="8" cy="9.5" r="1.8"/><path d="M5 18l5-6 4 4 3-3 4 5"/>',
    // 影片制作图标已随应用剥离：由应用包注册（runtimeIcons，registerRuntimeIcon）
    // 二维码传输图标已随应用剥离（apps/qrtransfer）：由应用包注册（runtimeIcons）
    devdocs:
        // 开发者中心：翻开的书 + 左右尖括号（开发者文档）
        '<path d="M3 5.5a1.5 1.5 0 0 1 1.5-1.5H10a2.5 2.5 0 0 1 2 .8 2.5 2.5 0 0 1 2-.8h5.5A1.5 1.5 0 0 1 21 5.5v12a1.5 1.5 0 0 1-1.5 1.5H14a2.5 2.5 0 0 0-2 .8 2.5 2.5 0 0 0-2-.8H4.5A1.5 1.5 0 0 1 3 17.5z"/>' +
        '<path d="M12 4.8v15"/>' +
        '<path d="M7 10l-1.5 1.5L7 13"/>' +
        '<path d="M17 10l1.5 1.5L17 13"/>',
    agenthub:
        // Agent 集合：机器人头像 + 下载箭头（与 DashboardView 内联 SVG 一致）
        '<path d="M12 8V5"/>' +
        '<circle cx="12" cy="4" r="1.2" fill="currentColor" stroke="none"/>' +
        '<rect x="4.5" y="8" width="15" height="11" rx="2.5"/>' +
        '<circle cx="9" cy="13" r="1.2" fill="currentColor" stroke="none"/>' +
        '<circle cx="15" cy="13" r="1.2" fill="currentColor" stroke="none"/>' +
        '<path d="M12 15v2.2"/>' +
        '<path d="M10.6 16.4l1.4 1.4 1.4-1.4"/>' +
        '<path d="M4.5 12H2.8"/>' +
        '<path d="M19.5 12h1.7"/>',
    terminal:
        // 管理（Web 终端）：命令行窗口 + `>_` 提示符（与 DashboardView 内联 SVG 一致）
        '<rect x="2.5" y="4" width="19" height="16" rx="2"/>' +
        '<path d="M2.5 8h19"/>' +
        '<circle cx="5.2" cy="6" r="0.55" fill="currentColor" stroke="none"/>' +
        '<circle cx="7.6" cy="6" r="0.55" fill="currentColor" stroke="none"/>' +
        '<path d="M6 12.2l2.6 2.3L6 16.8"/>' +
        '<path d="M10.6 17h4"/>',
    // live（直播）图标随流媒体中心剥离为应用包 apps/streaming
    // modelhub（模型仓库）已并入「模型管理」(/llm) 一级分组「仓库」，桌面图标/
    //   AppIcon 条目移除（此前 ICONS 即未单列 modelhub，桌面 SVG 在 DashboardView 内联）
}

// 查找顺序：内置 ICONS → 运行时应用包图标（runtimeIcons，响应式：注册即出现）
// → 调用方兜底（fallback，如通用 package 盒子图标——避免名字未命中时输出原始字符串）。
const inner = computed(() => ICONS[props.name] ?? runtimeIcons[props.name] ?? props.fallback)
</script>

<template>
    <svg
        xmlns="http://www.w3.org/2000/svg"
        :width="size"
        :height="size"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="1.5"
        stroke-linecap="round"
        stroke-linejoin="round"
        v-html="inner"
    />
</template>
