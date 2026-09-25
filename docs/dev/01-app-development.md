# 应用开发指南：新增一个桌面应用

> 目标：从零走通「新增 NexOS 桌面应用」的全流程——Vue 视图 → appRegistry 注册 →
> 路由 → 桌面图标 → API 客户端 → 构建。以「更新」应用（`/update`）为范例，
> 全部代码路径真实可查。用户高频问题「怎么让应用的图标在桌面显示」的答案
> 就是本文 §3–§4 两步（appRegistry + DashboardView 图标）。
>
> 前置：会 Vue3 `<script setup>` + TypeScript；已读 [README.md](../README.md)
> 协作铁律（功能文档同步：新功能必须写进对应 MD）。

## 0. 全流程一图

```text
① Vue 视图        web/src/views/Foo.vue           （页面本体）
② appRegistry     web/src/appRegistry.ts          （key→组件映射 + 桌面元信息 + 分类）★桌面图标第一步
③ router          web/src/router/index.ts         （/foo 路由 + 标题）
④ DashboardView   web/src/views/DashboardView.vue （allApps 数组 + 内联 SVG）★桌面图标第二步
⑤ AppIcon         web/src/components/AppIcon.vue  （ICONS 同款 SVG，Dock/状态栏复用）
⑥ client.ts       web/src/api/client.ts           （类型 + endpoints 调用封装）
⑦ 后端 handler    crates/os-api/src/handlers/foo.rs（见 06-os-api-handler.md）
⑧ 构建            npm run build → cargo build     （rust-embed 内嵌 static-dist/）
⑨ 文档            docs/FOO.md                     （功能文档同步铁律）
```

## 1. Vue 视图（web/src/views/）

新建 `web/src/views/Update.vue`（范例即此文件）。页面骨架约定：

```vue
<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { endpoints, type UpdateStatusResp } from '@/api/client';

const status = ref<UpdateStatusResp | null>(null);
onMounted(() => { void loadStatus(); });
</script>

<template>
  <div class="xxx-page">
    <div class="page-head"><div><h2>应用名</h2>…</div>…</div>
    …卡片内容…
  </div>
</template>
```

- 布局类用全局 `.page-head` / `.card`（`web/src/styles/main.css`）；
- 数据一律经 `endpoints.*`（§6），不裸写 fetch；
- 错误经 `friendlyError` 转中文提示（参考 `Update.vue` 顶部）。

## 2. appRegistry 注册（桌面图标·第一步）

`web/src/appRegistry.ts` 三处：

```ts
// ① key → 异步视图组件（key 同时是窗口 id）
export const appRegistry: Record<string, Component> = {
    update: defineAsyncComponent(() => import('@/views/Update.vue')),
    …
};

// ② 桌面元信息（Dock 栏 + 桌面图标共用）：id/icon 与 ① 的 key 一致
export const desktopApps: AppMeta[] = [
    { id: 'update', label: '更新', icon: 'update',
      gradient: 'linear-gradient(135deg, #f7971e 0%, #ffd200 100%)', route: '/update' },
    …
];

// ③ Launchpad 业务域分类（分组依据）
export const APP_CATEGORY: Record<string, AppCategory> = { …, update: 'power', … };
```

## 3. 路由（router/index.ts）

`web/src/router/index.ts` 主布局 children 里加一条（懒加载）：

```ts
{ path: 'update', name: 'update',
  component: () => import('@/views/Update.vue'),
  meta: { title: '更新', icon: 'update' } },
```

直接访问 `/update` 会被 `beforeEach` 守卫重定向到 `/?app=update`（桌面开浮窗），
无需自己处理。

## 4. DashboardView 图标（桌面图标·第二步）

`web/src/views/DashboardView.vue`：

1. `allApps` 数组加条目（label/route/gradient/icon，与 appRegistry ②一致）；
2. 图标区 `v-else-if` 链加一段内联 SVG（Yaru 风线条：`viewBox="0 0 24 24"`、
   `fill="none"`、`stroke="currentColor"`、`stroke-width="1.6"`），如 update 的
   环形双箭头：

```html
<svg v-else-if="app.icon === 'update'" class="app-svg" viewBox="0 0 24 24"
     fill="none" stroke="currentColor" stroke-width="1.6"
     stroke-linecap="round" stroke-linejoin="round">
    <path d="M4.5 12a7.5 7.5 0 0 1 13-5.1" />
    <path d="M17.5 4v3.2h-3.2" />
    <circle cx="12" cy="12" r="2.2" />
</svg>
```

3. 同款 SVG 路径补进 `web/src/components/AppIcon.vue` 的 `ICONS`（Dock/
   状态栏复用；两处必须一致）。

图标点击 → `openApp()` → `useWindowManager().openWindow({id: app.icon,…})`
→ WindowFrame 渲染 `appRegistry[win.id]`。

## 5. 客户端封装（client.ts）

`web/src/api/client.ts`：响应类型 + endpoint 方法（同源 fetch，`get/post` 封装）：

```ts
/** GET /api/v1/update/status 响应。 */
export interface UpdateStatusResp { current_version: string; channel: UpdateChannel; … }

export const endpoints = {
  …
  /** 更新总览（GET /api/v1/update/status）。 */
  updateStatus: (): Promise<UpdateStatusResp> => get<UpdateStatusResp>('/api/v1/update/status'),
};
```

## 6. 构建与嵌入

```bash
cd crates/os-api/web
npm run build        # vue-tsc -b && vite build → 产出 ../static-dist/
```

os-api 经 `crates/os-api/src/webui.rs` 的 rust-embed（`#[folder = "static-dist/"]`）
把产物打进二进制；**优先从磁盘读**（106 开发机改前端即时生效），无磁盘目录时
用内嵌副本（113/aliyun 部署形态）。改前端后重新 cargo 编译才会更新内嵌副本
（`cargo clean -p os-api` 强刷 rust-embed 缓存）。

## 7. 后端端点

`POST` 等写操作走 admin Bearer（`NEXOS_ADMIN_TOKEN`）；REST 契约见
`docs/UPDATE_APP.md`。新增 handler 的完整装配流程（mod.rs/main.rs/测试惯例）
见 [06-os-api-handler.md](06-os-api-handler.md)。

## 参考

- 范例全量代码：`crates/os-api/web/src/views/Update.vue`（读侧四区布局）
- 图标参考：`DashboardView.vue` 内 `<!-- xxx.svg -->` 注释链
- 文档铁律：`docs/README.md`（每个功能的新增能力和 env 必须写进对应 MD）
