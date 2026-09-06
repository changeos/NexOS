# NexOS Web UI 优化实施方案 v1.0

> 状态：草案 v1（待评审）
> 基线：源码 + 实机截图（`./_nexos_ui.png`，1280×720，Chrome 拉起的 NexOS Dashboard）
> 范围：仅 `crates/os-api/web/` 前端，**不**涉及 Rust 后端 API
> 原则：① 最小侵入（保留 fnOS 桌面范式）② 可灰度（每项可独立回滚）③ 视觉先于重构

---

## 0. 现状速览（已验证）

| 维度 | 现状 | 证据 |
|---|---|---|
| 技术栈 | Vue 3.5 + Vite 8 + TS + Pinia + vue-i18n + vue-router | `package.json` |
| 桌面范式 | 顶部状态栏(36px) + 资讯栏(28px) + 桌面图标 + 底部 Dock(120px zone) + 浮窗 | `MainLayout.vue` |
| 桌面图标 | 自由拖拽（`position:absolute`），非 CSS Grid；持久化到 `localStorage` | `DashboardView.vue` `.icon-grid` (line 1803) |
| 浮窗 | 自由拖拽 + Aero Snap + 缩放 + 最小 320×240 | `WindowFrame.vue` |
| 设计 tokens | **仅浅色**一套（Yaru 浅色 + Ubuntu Orange 强调） | `tokens.css` |
| 壁纸 | 4 张 AI 图 + 6 套 CSS 渐变，默认 `nexos-aubergine`（深紫） | `useWallpaper.ts` |
| 桌面应用 | **29 个**（异步加载），Dock/桌面互斥布局 | `appRegistry.ts` |
| 系统磁贴 | 已支持 48×48 收起环 ↔ 200×320 展开卡，`position:fixed`，位置/展开态持久化 | `SystemWidget.vue` + `useWidgetState` |
| 响应式断点 | 仅 900/560 两条 | `MainLayout.vue` media queries |

---

## 1. 优化目标 & 设计原则

**目标**（按 BP 路演与产品定位倒推）
1. 修掉截图里**肉眼可见的 4 个 bug**（P0）
2. 把调性从"有机+彩虹"拉到**"科技/科幻风"**——对齐融资 BP 调性（P1）
3. 为后续 29+ 视图的**一致性**打底（P2，本期不展开）

**设计原则**
- **桌面范式不动**：保留 fnOS/macOS 风的"图标 + Dock + 浮窗"，这是产品辨识度。
- **系统磁贴是"浮窗"不是"面板"**：它属于用户可随手收起/拖动的小物，不该默认霸占桌面。
- **资讯栏 = 事件流，不是广告位**：常驻内容必须是真实系统信号。
- **29 个应用必须有"总览入口"**：否则桌面/Dock 任意一端都承载不了。

---

## 2. P0 详细方案（必修，4 项）

### P0-1 系统磁贴不挡桌面图标

**问题**（截图实证）：`SystemWidget` 默认展开态 `position:fixed; left:?; top:?` 直接覆盖右半屏图标列（系统监控/文件管理/容器管理被压）。

**根因**：`SystemWidget` 默认状态 `expanded: true`（`useWidgetState` 初值），且无边缘吸附。

**改法**（3 步）：
1. **`useWidgetState` 初值改 `expanded: false`**，首次访问默认收起成右下角 48×48 圆环。
   - 文件：`composables/useWidgetState.ts`（推断结构，按实际微调）
   - 影响：老用户 localStorage 已有 `expanded:true`，需在 `ensureInit()` 里加一次**"一次性迁移"**：若旧值无 `expanded` 字段，置 `false`；版本号 `version=2` 持久化。
2. **`SystemWidget` 加边缘吸附**：拖拽 `mouseup` 时（`onDragEnd`，line 175-182），计算到四边距离，吸附到最近边（留 8px 边距），展开态自动靠右或靠下。
3. **桌面图标拖拽约束**避开磁贴展开区：
   - 在 `MainLayout.vue` 暴露 `--widget-safe-zone` CSS 变量（`useWidgetState` 监听 expanded 切换 class）。
   - `DashboardView.vue` 的 `onDragMove` / 自动布局函数读该变量，约束 `x` 范围 = `0 .. (desktopWidth - iconW - safeRight)`。
   - 文件：`DashboardView.vue` 拖拽逻辑段（约 line 525 `querySelector('.desktop-root')` 附近）

**改动量**：S（~60 行）
**风险**：低——仅改默认值 + 边缘吸附 + 拖拽约束；老用户位置/展开态保留。
**回滚点**：`useWidgetState` 改回 `expanded: true`。

---

### P0-2 资讯栏去营销化

**问题**（截图实证）：常驻播"支持 ZFS/虚拟机/Docker""内置 Qwen3-VL 推理""端到端加密、多用户权限、数据安全有保障"——是 BP 路演话术，不是系统事件。

**改法**（`MainLayout.vue` news-ticker 段，line 258-265 + 175-191）：
1. **数据源改造**：`/api/v1/system/ads` → `/api/v1/system/notifications`（若后端暂无，先用占位端点 + 静态 JSON）。
   - 字段：`{ id, level: 'info'|'warn'|'critical', text, ts, dismissable }`
   - 优先级：`critical > warn > info`，`critical` 默认展开，其余折叠到铃铛角标。
2. **样式改造**：
   - 去掉 📢 emoji；标题改为"通知"（或 i18n key `ticker.title`）。
   - 颜色：critical 用 `--err`，warn 用 `--warn`，info 用白。
   - 高度 28px → 24px，hover 展开多行。
3. **可关闭**：右侧 × 按钮，关闭后写 `localStorage ticker-dismissed-ts`，24h 内不再展开（或直接折叠成右上角铃铛角标）。
4. **轮询频率**：15s → 30s，且 `document.visibilityState === 'hidden'` 时停轮询（用 `visibilitychange` 事件）。

**改动量**：S（~50 行）
**风险**：低——纯前端改造，后端端点可后续接。
**回滚点**：保留旧 `ads` 端点调用作 fallback。

---

### P0-3 Dock 增加 Launchpad 入口

**问题**（截图实证）：底部 Dock 只一个孤零零的齿轮图标，29 个应用没入口。

**改法**：
1. **新增组件** `components/LaunchpadOverlay.vue`（仿 macOS Launchpad）：
   - 全屏浮层 `position:fixed; inset:0; z-index: 200`
   - 背景：壁纸 + 80% 暗化 + `backdrop-filter: blur(20px)`
   - 顶部搜索框（实时过滤 `desktopApps` 标签/分类）
   - 主体：`grid-template-columns: repeat(auto-fill, 96px)`，每个 app = 图标 + 标签
   - 关闭：ESC / 点空白 / ×
2. **Dock 注册 Launchpad 入口**：
   - 在 `appRegistry.ts` 的 `desktopApps` 前面 push 一项：
     ```ts
     { id: 'launchpad', label: '应用总览', icon: 'launchpad',
       gradient: 'linear-gradient(135deg,#E95420,#772953)',
       route: '/launchpad', isLauncher: true }
     ```
   - Dock 渲染时：`isLauncher` 项不进入"打开浮窗"分支，点击触发 `LaunchpadOverlay.open()`。
   - 用一个轻量全局 store（或 `mitt` 事件）通信。
3. **Dock 默认项**：若用户从未配置 Dock，给一组"高频默认"：存储管理 / 系统监控 / 备份管理 / 启动台 / 设置。即默认 dock 不少于 4 项，避免空荡。

**改动量**：M（新增 ~200 行 LaunchpadOverlay + Dock 注册 ~30 行 + 默认 dock 逻辑 ~30 行）
**风险**：低——纯新增；旧行为（空 dock + 全部在桌面）保留为可选项。
**回滚点**：Dock 默认值不写死，只新增 Launchpad 入口。

---

### P0-4 桌面图标网格 safe-area

**问题**（截图实证）：`.desktop-root` 满屏铺开，`.icon-grid` `width:100%; height:100%`（`DashboardView.vue` line 1803），图标 `position:absolute` 自由定位——没人管 widget 在哪。

**改法**：
- 在 `MainLayout.vue` 的 `.desktop-area` 上根据 `useWidgetState` 的 `expanded + 位置` 计算并写入 CSS 变量：
  ```css
  .desktop-area { --safe-right: 8px; --safe-bottom: 92px; --safe-top: 64px; --safe-left: 8px; }
  .desktop-area[data-widget-expanded][data-widget-side="right"] { --safe-right: 224px; }
  .desktop-area[data-widget-expanded][data-widget-side="left"]  { --safe-left:  224px; }
  .desktop-area[data-widget-expanded][data-widget-side="bottom"] { --safe-bottom: 344px; }
  ```
- `DashboardView.vue` 拖拽与首次自动布局时，把 `clientWidth/clientHeight` 减去对应 safe 值，约束 `x/y`。
- `MainLayout.vue` 监听 `useWidgetState` 的变化，给 `.desktop-area` 同步 `data-widget-expanded` / `data-widget-side`。

**改动量**：S（~40 行 CSS 变量 + ~30 行 TS 约束）
**风险**：中——约束逻辑要保证老用户已有图标位置在重新拖拽前不"跳"。**方案**：只在**拖拽期间**+**新自动布局**时应用约束；老位置加载时 clamp 到新范围（不删除，仅夹紧），下次保存即生效。
**回滚点**：CSS 变量 `--safe-*` 全部回 8px 即可。

---

## 3. P1 详细方案（调性升级，4 项）

### P1-1 Dark 主题 tokens（让浮窗/卡片/表格也能切深色）

**问题**：`tokens.css` 仅浅色一套；浮窗永远浅白 + 桌面深紫，**调性割裂**。BP 路演要求"科技/科幻风"必须能整页深色。

**改法**（`styles/tokens.css` + `styles/tokens-dark.css` 新文件）：
```css
/* tokens-dark.css —— 科技/科幻风（深空蓝紫 + 霓虹橙） */
:root[data-theme="dark"] {
    --bg-app:   #0B0E1A;   /* 深空底 */
    --bg-card:  #12172A;   /* 卡片 */
    --bg-elev:  #1A2040;   /* 表头/h */
    --text:       #E6E8F0;
    --text-muted: #8A8FB0;
    --text-faint: #5A5F7A;
    --border:     #2A3050;
    --border-soft:#1F2540;
    --hairline:   rgba(255,255,255,0.06);
    --accent:     #E95420;   /* 保留 Ubuntu Orange */
    --accent-hi:  #FF6A33;
    --accent-soft:rgba(233,84,32,0.18);
    --neon-cyan:  #30D5FF;   /* 新增 */
    --neon-violet:#9B6BFF;   /* 新增 */
    /* 状态色 */
    --ok:   #4CD964;
    --warn: #FFB020;
    --err:  #FF5470;
    --info: #30D5FF;
    /* 玻璃效果（dark 下用更黑的玻璃） */
    --glass-bg: rgba(11,14,26,0.72);
    --glass-blur: blur(16px);
    /* 阴影（dark 下更深） */
    --shadow:      0 1px 2px rgba(0,0,0,0.4), 0 4px 12px rgba(0,0,0,0.32);
    --shadow-lg:   0 8px 24px rgba(0,0,0,0.48);
    --shadow-modal: 0 20px 60px rgba(0,0,0,0.6);
}
```
- 主题切换：`html` 根设 `data-theme="dark|light"`，写 `localStorage 'os-theme'`，默认 `light`。
- `MainLayout.vue` 在 `onMounted` 读 `os-theme` 写到 `<html data-theme="...">`。
- `WindowFrame.vue` / `SystemWidget.vue` / 卡片等所有 `var(--bg-card)` / `var(--text)` 等自动跟随，**无需逐文件改**。
- `tokens.css` 注释更新，明确："light 是默认；dark 见 tokens-dark.css"。

**改动量**：M（新增 ~80 行 + `MainLayout` 加 ~15 行 + 切换入口放在 Settings 视图）
**风险**：中——深色下表格 `tbody tr:hover { background: rgba(0,0,0,0.03) }`（`main.css` line 138）需改为浅色高亮（`rgba(255,255,255,0.05)`），用 `:root[data-theme="dark"]` 覆盖。
**回滚点**：不写 `data-theme="dark"` 即回退。

---

### P1-2 科技风壁纸预设

**改法**（`composables/useWallpaper.ts` `WALLPAPERS` 数组追加 3 项）：
```ts
{
    id: 'nexos-nebula',
    name: '深空星云',
    css: 'radial-gradient(ellipse 60% 50% at 20% 20%, rgba(48,213,255,0.18), transparent 60%),'
       + 'radial-gradient(ellipse 50% 40% at 80% 80%, rgba(155,107,255,0.18), transparent 60%),'
       + 'linear-gradient(135deg, #050818 0%, #0B0E1A 50%, #12172A 100%)',
    preview: 'linear-gradient(135deg, #050818, #12172A)',
    textLight: true,
},
{
    id: 'nexos-grid',
    name: '数据网格',
    css: 'linear-gradient(rgba(48,213,255,0.06), rgba(48,213,255,0.04)),'
       + 'repeating-linear-gradient(0deg, rgba(48,213,255,0.08) 0 1px, transparent 1px 64px),'
       + 'repeating-linear-gradient(90deg, rgba(48,213,255,0.08) 0 1px, transparent 1px 64px),'
       + 'radial-gradient(ellipse 70% 60% at 50% 50%, rgba(11,14,26,0.6), transparent 70%),'
       + 'linear-gradient(135deg, #0B0E1A 0%, #12172A 100%)',
    preview: 'linear-gradient(135deg, #0B0E1A, #12172A)',
    textLight: true,
},
{
    id: 'nexos-flux',
    name: '霓虹流光',
    css: 'linear-gradient(rgba(11,14,26,0.62), rgba(11,14,26,0.72)),'
       + 'conic-gradient(from 210deg at 50% 50%, #30D5FF, #9B6BFF, #E95420, #30D5FF),'
       + 'linear-gradient(135deg, #0B0E1A, #12172A)',
    preview: 'linear-gradient(135deg, #0B0E1A, #12172A)',
    textLight: true,
},
```
- 默认壁纸改为 **`nexos-grid`**（数据网格最"科技"），老用户保留各自选择。
- 设置面板（`Settings.vue`）已有壁纸切换时，新增 3 项直接出现在网格里。

**改动量**：XS（~30 行）
**风险**：极低——纯新增，老 id 不动。
**回滚点**：默认 id 改回 `nexos-aubergine`。

---

### P1-3 图标配色收敛（去"彩虹糖"）

**问题**（截图实证）：29 个图标每个独立高饱和高反差渐变，视觉噪音大。

**改法**（`appRegistry.ts` `desktopApps[].gradient` 收敛）：
- 制定**分类色板**：
  | 类别 | 代表应用 | 渐变 |
  |---|---|---|
  | 基础设施 | storage/vm/containers/network/provisioning | 橙紫（`#E95420→#772953`） |
  | 数据服务 | backup/files/notes/qrtransfer | 蓝青（`#30D5FF→#1B3A5C`） |
  | 媒体 | video/music/photo/streaming | 紫青（`#9B6BFF→#30D5FF`） |
  | 智能 | llm/modelhub/chat | 霓虹橙（`#E95420→#30D5FF`） |
  | 安全监控 | monitor/surveillance/security | 青绿（`#4CD964→#1B3A5C`） |
  | 集群管理 | nodes/codehub/gateway | 冷灰蓝（`#5A5F7A→#2A3050`） |
  | 区块链 | blockchain | 深紫（`#9B6BFF→#0B0E1A`） |
- 同时**降饱和度**：去掉 `linear-gradient(135deg,#fa709a,#fee140)` 这类高饱和高反差，统一为深-深 135deg。
- 图标尺寸：tile 52×52 → 56×56（视觉更稳），radius 18 → 16（更"科技"）。

**改动量**：S（~30 行 appRegistry 改 + ~10 行 DashboardView 样式）
**风险**：低——纯视觉，用户可立刻看出新调性。
**回滚点**：git revert。

---

### P1-4 状态栏补齐（时钟/网络/通知/用户）

**问题**（截图实证）：`MainLayout.vue` 状态栏只有 `NexOS` + 绿点 + 齿轮，时钟挤在 widget 里导致 widget 必须展开。

**改法**（`MainLayout.vue` `.statusbar` 段，line 231-255）：
```
[sb-left]  NexOS › 活跃窗口标题          [sb-right] CPU% │ MEM% │ 网络 ↑↓ │ 🔔 │ 👤 admin │ ⚙
```
- 中间加 `NetworkBadge.vue` / `ResourceBadge.vue`（小芯片式，hover 弹浮层）
- 🔔 通知入口：点击打开通知中心（复用 P0-2 的 notifications 数据源）
- 👤 当前用户（从 store 读）
- 时钟从 SystemWidget 移到状态栏（`HH:MM`，秒数 hover 才显示），让 widget 默认 compact 即可。

**改动量**：M（~80 行 MainLayout + 2 个小组件）
**风险**：中——状态栏变密，需要保证 1280px 起步不挤；窄屏隐藏中间项。
**回滚点**：状态栏可折叠控制（开关项放 Settings）。

---

## 4. 执行顺序与里程碑

| 阶段 | 内容 | 预计 | 验证 |
|---|---|---|---|
| **M1** | P0-1 widget 默认收起 + 边缘吸附 | 0.5d | 截图：默认看不到 widget，拖到底边自动吸附 |
| **M1** | P0-4 桌面 safe-area | 0.5d | 截图：widget 展开时桌面图标不再被压 |
| **M2** | P0-2 资讯栏去营销 | 0.5d | 截图：仅显示真实通知/告警 |
| **M2** | P0-3 Launchpad 入口 | 1d | 截图：点启动格 → 29 个应用总览 + 搜索 |
| **M3** | P1-2 新增科技风壁纸 | 0.2d | 设置面板出现 3 个新预设 |
| **M3** | P1-1 Dark tokens | 1d | 切换主题：浮窗/卡片/表格全变深色，截图无破图 |
| **M3** | P1-3 图标配色收敛 | 0.5d | 截图：图标从"彩虹"变"分类色板" |
| **M3** | P1-4 状态栏补齐 | 0.8d | 截图：状态栏含时钟/网络/通知/用户 |

**总工时估算**：~5 人日（1 人 1 周可完成全部 P0+P1）。

---

## 5. 风险 & 兼容

| 风险 | 等级 | 缓解 |
|---|---|---|
| 老用户 localStorage 图标位置在新 safe-area 下被 clamp 后视觉跳 | 中 | 仅 clamp 不删除；首次拖拽后保存新值；提供"重置布局"按钮 |
| Dark 主题下深色 SVG 图标不可见 | 中 | `AppIcon.vue` 抽 `--icon-color` 变量；深色主题下统一反色；逐个图标审 |
| 资讯栏改成通知流后短期无真实数据，看着"空" | 低 | 默认显示一条"系统正常运行中"info 级空态 |
| Launchpad 在 29 个应用下性能 | 低 | `desktopApps` 仅 29 个，DOM 渲染无压力；`v-memo` 防重渲 |
| Wallpaper 渐变更深 → 桌面图标白字可能刺眼 | 低 | `app-label` 已有 `text-shadow` 兜底；深色主题下统一降不透明度到 0.92 |

---

## 6. 回滚策略

- 每项改动**独立 commit**，可在 PR 级别回滚。
- `useWidgetState` 加 `version` 字段，schema 变更时一次性迁移；不一致时回退到默认。
- Dark 主题开关写在 `localStorage` + `html[data-theme]`，去掉 `data-theme` 立刻回退到 light。
- 壁纸预设纯新增，不删老 id。
- 图标配色收敛：若用户反馈强烈，`appRegistry` gradient 字段可改成支持"调色板 token"（`palette: 'infra'` 之类），一处定义多套可选。

---

## 7. 需要你决策的项

1. **默认壁纸** 改为 `nexos-grid`（数据网格）你接受吗？还是要保留 `nexos-aubergine`（流光紫韵）？
2. **默认主题** 选 `light` 还是 `dark`？（建议默认 `light` 保留传统 NAS 调性，dark 作为可选；路演/截图时切 dark）
3. **资讯栏** 是否允许"完全关闭"（设选项），还是永远留一个角标？
4. **Launchpad** 的"分类"维度怎么分？我提议：基础设施 / 数据服务 / 媒体 / 智能 / 安全监控 / 集群管理 / 区块链 / 其它（8 类），需要你确认分类边界（比如"API 网关"算集群还是基础？）。
5. **P1-3 图标配色收敛** 是否要兼顾**保留原 29 种渐变**作为"个性化"选项？我倾向直接换，仅在 Settings 加"图标风格"切换（默认 = 收敛版，高级 = 原始版）。

---

## 8. 不在本次范围（后续 P2 候选）

- 29+ 业务视图抽公共 `Card / StatTile / EmptyState / SectionHeader`
- 响应式断点补齐（1024 / 768 浮窗降级为全屏栈）
- 可访问性（focus-visible、键盘可达、状态色配文字）
- 命名收敛收尾（注释/文案清旧 "DSM" 痕迹）
- 国际化 i18n 收口（vue-i18n 已接，三语覆盖度核查）

---

_文档结束。改动会按 P0 → P1 顺序逐项 PR，每项 PR 自带"改前 vs 改后"截图对比。_

---

## 9. 决策确认（2026-08-22）

经逐项确认，5 项决策全部通过，方向如下：

| # | 决策项 | 结论 |
|---|---|---|
| 1 | 推进节奏 | 先逐项确认 5 项再改；确认后"全部通过，开始落地 P0" |
| 2 | 提交身份 | 沿用临时优化身份：`NexOS UI Optimizer <ui-optimizer@nexos.local>` |
| 3 | 默认壁纸 | 改为科幻风新壁纸 `nexos-cyber`（赛博矩阵）；新增 `nexos-nebula`（星云深空）备用 |
| 4 | 默认主题 | 浅色为默认；深色调作为 P1 预留（tokens.dark.css + 切换） |
| 5 | Launchpad 分类 | 采用 8 类：备份 / 监控 / 媒体 / 安全 / 文件 / 开发者工具 / 电源·基础设施 / 系统（见 `appRegistry.APP_CATEGORY` / `CATEGORY_META`） |

> 说明：P1 项（深色调、图标配色收敛）本次**未**实施，保留为后续迭代。

## 10. P0 落地状态（2026-08-22）

P0 全部代码改动已完成，并经 `npm run build`（vue-tsc + vite）验证通过（`BUILD_EXIT=0`）。

| 改动文件 | 类型 | 内容 |
|---|---|---|
| `composables/useWidgetState.ts` | 改 | 系统组件默认 `expanded: false`，解决默认展开遮挡桌面图标 |
| `components/SystemWidget.vue` | 改 | 拖拽结束边缘吸附（贴左/贴右），避免悬停遮挡 |
| `views/DashboardView.vue` | 改 | `.desktop-root` 底部 padding 24→140px，避让 Dock 区 |
| `composables/useWallpaper.ts` | 改 | 新增 `nexos-cyber`/`nexos-nebula` 两套科幻壁纸；默认壁纸改 `nexos-cyber` |
| `layouts/MainLayout.vue` | 改 | 状态栏新增"启动台"入口；资讯栏可关闭（localStorage 记忆）；新增通知中心面板 + 铃铛角标 |
| `appRegistry.ts` | 改 | 新增 `APP_CATEGORY` / `CATEGORY_META`（业务域分类，供 Launchpad 分组） |
| `components/Launchpad.vue` | 新增 | 全屏应用启动台，按 8 类业务域分组，点击即开窗口/路由；Esc / 点遮罩关闭 |

提交分支：`feature/ui-optimization`；构建产物：`crates/os-api/static-dist/`。
