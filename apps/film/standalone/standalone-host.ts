// =============================================================================
// standalone-host.ts —— 应用包「独立运行宿主」（应用包自给自足，脱离 NexOS 桌面壳）。
//
// 桌面嵌入模式：宿主桥 window.__NEXOS_HOST__={vue,vueI18n,api} 由主前端
// appRuntime.ts 提供（应用包不打包 vue，见 vite.config.ts host-externals）。
// 独立模式（本文件）：standalone-host 就是真宿主——vite lib 构建把
// vue + vue-i18n 完整打进本产物（~150KB 量级，应用包从此完全自包含，
// 内网离线可跑，不引 CDN），然后：
//   1. 置 window.__NEXOS_STANDALONE__ = true（应用 UI 据此隐藏外链图标等）；
//   2. 以极简 ctx 适配器调 register（registerApp→createApp 挂 #app 全屏，
//      addRoute→noop，addI18n→注入独立 i18n 实例，api→fetch 原语）；
//   3. api 语义与主前端 client.ts 对齐：同源路径、JSON、Bearer token
//      （localStorage 'os-api-token'，与桌面设置页同 key 同源共享）、15s 超时、
//      非 2xx 抛 ApiError（message/status/path）。写操作遇 401/403 弹极简
//      token 输入条（记住后自动重试一次）。
// =============================================================================

// —— 1. 独立模式标记（尽早置位；宿主桥在 api 原语定义后安装，见下）——
;(globalThis as { __NEXOS_STANDALONE__?: boolean }).__NEXOS_STANDALONE__ = true

import * as vue from 'vue'
import * as vueI18n from 'vue-i18n'
import { createSdk } from '@nexos/app-sdk'
import registerFilm from '../src/entry'

// =============================================================================
// api 原语（与主前端 crates/os-api/web/src/api/client.ts 语义对齐）
// =============================================================================

const TIMEOUT_MS = 15_000
/** 与主前端 client.ts 同 key——桌面「设置 → API 令牌」填过即全局生效（同源共享）。 */
const TOKEN_STORAGE_KEY = 'os-api-token'

/** API 错误（携带 HTTP status + path，与主前端 ApiError 同形态，便于 UI 展示）。 */
class ApiError extends Error {
  status?: number
  path: string
  constructor(message: string, init?: { status?: number; path?: string }) {
    super(message)
    this.name = 'ApiError'
    this.path = init?.path ?? ''
    if (init?.status !== undefined) this.status = init.status
  }
}

function getApiToken(): string {
  try {
    return localStorage.getItem(TOKEN_STORAGE_KEY) ?? ''
  } catch {
    return ''
  }
}

function setApiToken(v: string): void {
  try {
    const t = (v ?? '').trim()
    if (t) localStorage.setItem(TOKEN_STORAGE_KEY, t)
    else localStorage.removeItem(TOKEN_STORAGE_KEY)
  } catch {
    /* 隐私模式等：忽略，仅本次内存生效 */
  }
}

/** 极简 token 输入条（固定底栏）：401/403 时出现，返回用户输入（跳过返回 null）。 */
function promptToken(path: string, status: number): Promise<string | null> {
  return new Promise((resolve) => {
    const prev = document.getElementById('nexos-standalone-tokenbar')
    if (prev) prev.remove()
    const bar = document.createElement('div')
    bar.id = 'nexos-standalone-tokenbar'
    bar.style.cssText = [
      'position:fixed', 'left:0', 'right:0', 'bottom:0', 'z-index:9999',
      'display:flex', 'gap:8px', 'align-items:center', 'flex-wrap:wrap',
      'padding:10px 14px', 'background:#1f2028', 'color:#e6e4e9',
      'border-top:1px solid #E95420', 'font:13px/1.4 system-ui, sans-serif',
    ].join(';')
    const label = document.createElement('span')
    label.textContent = `管理 token（${status}，${path}）：`
    const input = document.createElement('input')
    input.type = 'password'
    input.placeholder = '粘贴 admin token（记住到本浏览器）'
    input.style.cssText =
      'flex:1;min-width:200px;padding:6px 9px;border:1px solid #3a3c47;border-radius:6px;background:#16171d;color:#e6e4e9;font:inherit'
    const ok = document.createElement('button')
    ok.textContent = '保存并重试'
    ok.style.cssText =
      'padding:6px 12px;border:none;border-radius:6px;background:#E95420;color:#fff;font:inherit;cursor:pointer'
    const skip = document.createElement('button')
    skip.textContent = '跳过'
    skip.style.cssText =
      'padding:6px 12px;border:1px solid #3a3c47;border-radius:6px;background:transparent;color:#9ca3af;font:inherit;cursor:pointer'
    bar.append(label, input, ok, skip)
    document.body.appendChild(bar)
    input.focus()
    const done = (v: string | null) => {
      bar.remove()
      resolve(v)
    }
    const submit = () => done(input.value.trim() || null)
    ok.onclick = submit
    skip.onclick = () => done(null)
    input.onkeydown = (ev) => {
      if (ev.key === 'Enter') submit()
      if (ev.key === 'Escape') done(null)
    }
  })
}

/** 统一 fetch：同源 JSON；非 2xx 抛 ApiError；401/403 可弹 token 条重试一次。 */
async function request<T>(
  path: string,
  opts?: { method?: string; body?: unknown },
  allowTokenRetry = true,
): Promise<T> {
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(), TIMEOUT_MS)
  const headers: Record<string, string> = { Accept: 'application/json' }
  const token = getApiToken()
  if (token.trim()) headers.Authorization = `Bearer ${token}`
  const init: RequestInit = {
    method: opts?.method ?? 'GET',
    headers,
    signal: controller.signal,
  }
  if (opts?.body !== undefined) {
    init.body = JSON.stringify(opts.body)
    headers['Content-Type'] = 'application/json'
  }
  let resp: Response
  try {
    resp = await fetch(path, init)
  } catch (e) {
    clearTimeout(timer)
    const reason = e instanceof Error && e.name === 'AbortError' ? `超时（${TIMEOUT_MS / 1000}s）` : String(e)
    throw new ApiError(`请求失败：${reason}`, { path })
  }
  clearTimeout(timer)

  // 写操作 401/403 → 极简 token 输入条（记住后重试一次）
  if ((resp.status === 401 || resp.status === 403) && allowTokenRetry) {
    const t = await promptToken(path, resp.status)
    if (t) {
      setApiToken(t)
      return request<T>(path, opts, false)
    }
  }

  if (!resp.ok) {
    let detail = ''
    try {
      const body = (await resp.json()) as { error?: string; message?: string }
      detail = (body && (body.error || body.message)) || JSON.stringify(body)
    } catch {
      detail = ''
    }
    if (resp.status === 401) {
      throw new ApiError('未授权（401）——请提供管理员 token 后重试', { status: resp.status, path })
    }
    throw new ApiError(`${resp.status} ${resp.statusText}${detail ? ' — ' + detail : ''}`, {
      status: resp.status,
      path,
    })
  }
  if (resp.status === 204) return undefined as T
  const text = await resp.text()
  if (!text) return undefined as T
  try {
    return JSON.parse(text) as T
  } catch {
    return text as unknown as T
  }
}

const api = {
  get: <T>(path: string): Promise<T> => request<T>(path),
  post: <T>(path: string, body?: unknown): Promise<T> =>
    request<T>(path, { method: 'POST', body }),
  del: <T>(path: string): Promise<T> => request<T>(path, { method: 'DELETE' }),
  request: <T>(path: string, opts?: { method?: string; body?: unknown }): Promise<T> =>
    request<T>(path, opts),
}

// —— 2. 宿主桥自给自足：桥协议与主前端 appRuntime.ts 相同（__NEXOS_HOST__={vue,vueI18n,api,sdk}）。
//    应用的 src/api.ts 直接消费 __NEXOS_HOST__.api——必须先于 register 装好。
//    sdk（2026-09-04 v0.1.28）：@nexos/app-sdk 就绪实例（独立模式 SDK 源码经
//    vite resolve.alias 指向主前端 crates/os-api/web/src/sdk/ 唯一事实源打进
//    本产物——standalone 自包含；getToken 用本文件同一 localStorage key）。
//    通知策略：独立模式走 Notification API（已授权时），无权限降级 SDK 自绘
//    迷你 toast（notify.ts 三档策略，与桌面嵌入模式的主前端 toast 相区分）。
const sdk = createSdk(api, { getToken: getApiToken })
;(globalThis as unknown as { __NEXOS_HOST__: unknown }).__NEXOS_HOST__ = {
  vue,
  vueI18n,
  api,
  sdk,
}

// =============================================================================
// ctx 适配器 + 装载
// =============================================================================

/** 基础样式底：桌面 tokens.css 的暗色近似（film 组件全部 var(…, fallback) 消费）。 */
function injectBaseStyles(): void {
  if (typeof document === 'undefined') return
  if (document.getElementById('nexos-standalone-base')) return
  const style = document.createElement('style')
  style.id = 'nexos-standalone-base'
  style.textContent = `
:root {
  color-scheme: dark;
  --bg: #16171d; --bg-app: #16171d; --bg-card: #1f2028; --bg-elev: #23242e;
  --text: #e6e4e9; --text-h: #f3f4f6; --text-muted: #9ca3af; --text-faint: #6b7280;
  --accent: #E95420; --accent-hi: #F0633A; --accent-soft: rgba(233, 84, 32, 0.18);
  --border: #3a3c47; --border-soft: #2e303a; --hairline: rgba(255, 255, 255, 0.08);
  --radius: 10px; --radius-md: 8px; --radius-sm: 6px; --radius-pill: 16px;
  --shadow: 0 1px 2px rgba(0, 0, 0, 0.4), 0 2px 6px rgba(0, 0, 0, 0.25);
  --shadow-modal: 0 12px 40px rgba(0, 0, 0, 0.5);
  --mono: "Ubuntu Mono", ui-monospace, Menlo, Consolas, "Liberation Mono", monospace;
}
html, body, #app { height: 100%; }
body {
  margin: 0;
  background: var(--bg);
  color: var(--text);
  font-family: "Ubuntu", -apple-system, BlinkMacSystemFont, "Segoe UI",
    "PingFang SC", "Microsoft YaHei", "Noto Sans CJK SC", sans-serif;
  -webkit-font-smoothing: antialiased;
}
`
  document.head.appendChild(style)
}

/** 受支持的四语言（与 i18n/ 目录一致）。 */
type Locale = 'zh-CN' | 'zh-TW' | 'en-US' | 'ja-JP'

/** 语言判定：localStorage(os.locale)（与桌面同 key 同源共享）→ navigator.language → zh-CN。 */
function detectLocale(): Locale {
  try {
    const saved = localStorage.getItem('os.locale')
    if (saved === 'zh-CN' || saved === 'zh-TW' || saved === 'en-US' || saved === 'ja-JP') {
      return saved
    }
  } catch {
    /* ignore */
  }
  const nav = (typeof navigator !== 'undefined' && navigator.language) || 'zh-CN'
  const low = nav.toLowerCase()
  if (low.startsWith('zh-tw') || low.startsWith('zh-hant') || low === 'zh-hk') return 'zh-TW'
  if (low.startsWith('ja')) return 'ja-JP'
  if (low.startsWith('en')) return 'en-US'
  return 'zh-CN'
}

/** 致命错误兜底（register/挂载失败时给可读页面，不白屏）。 */
function showFatal(err: unknown): void {
  if (typeof document === 'undefined') {
    console.error('[nexos-app-film standalone]', err)
    return
  }
  const msg = err instanceof Error ? err.message : String(err)
  const app = document.getElementById('app')
  if (app) {
    app.innerHTML = `
      <div style="max-width:560px;margin:15vh auto 0;padding:0 20px;font:14px/1.7 system-ui,sans-serif;color:#e6e4e9">
        <h2 style="font-size:18px;margin:0 0 10px;color:#E95420">影片制作（独立模式）装载失败</h2>
        <p style="margin:0 0 8px;word-break:break-all">${msg.replace(/[<>&]/g, (c) => ({ '<': '&lt;', '>': '&gt;', '&': '&amp;' })[c] ?? c)}</p>
        <p style="margin:0;color:#9ca3af">请确认 NexOS os-api 服务可达后刷新重试。</p>
      </div>`
  }
}

async function main(): Promise<void> {
  injectBaseStyles()

  // —— ctx 适配器（register(ctx) 冻结契约的最小实现）——
  let appDecl: {
    label?: string
    component?: unknown
  } | null = null
  const messages: Partial<Record<Locale, Record<string, unknown>>> = {}

  const ctx = {
    registerApp(decl: { label?: string; component?: unknown }): void {
      appDecl = decl
    },
    addRoute(_route: unknown): void {
      /* 独立模式无桌面路由树——noop */
    },
    addI18n(locale: Locale, msgs: Record<string, unknown>): void {
      messages[locale] = msgs
    },
    api,
    sdk,
  }

  await registerFilm(ctx as Parameters<typeof registerFilm>[0])

  // 回调内赋值不被 TS 追踪——快照后判空
  const decl = appDecl as { label?: string; component?: unknown } | null
  if (!decl || !decl.component) {
    throw new Error('register 未提供 component（registerApp 缺失）')
  }
  if (decl.label && typeof document !== 'undefined') document.title = `${decl.label} · NexOS`

  const i18n = vueI18n.createI18n({
    legacy: false,
    locale: detectLocale(),
    fallbackLocale: 'zh-CN',
    messages: messages as unknown as Record<string, Record<string, string>>,
    missingWarn: false,
    fallbackWarn: false,
  })
  const app = vue.createApp(decl.component as vue.Component)
  app.use(i18n)
  app.mount('#app')
}

main().catch(showFatal)
