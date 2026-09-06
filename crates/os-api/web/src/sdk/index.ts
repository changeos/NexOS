// =============================================================================
// index.ts —— @nexos/app-sdk 入口（createSdk）。
//
// NexOS 应用能力面 SDK：应用（含第三方应用包）以三行代码接入 NexOS 能力
// （联邦大厅 / API 网关 / 本地 LLM / 通知），独立运行时按能力快照降级
// （docs/APPS.md「应用 SDK」章）。
//
// # 载体与复用机制（重要）
//
// 唯一事实源 = 本目录（crates/os-api/web/src/sdk/*.ts）。两条消费路径：
//   1. 主前端：appRuntime.ts 直接相对导入（./sdk），createSdk(宿主 api 原语)
//      后挂到宿主桥 window.__NEXOS_HOST__.sdk，并经 ctx.sdk 下发每个应用；
//   2. 应用包构建期：vite host-externals 把 `import { createSdk } from
//      '@nexos/app-sdk'` 重写到 __NEXOS_HOST__.sdk（零打包）；standalone
//      宿主（无主前端壳）经 resolve.alias 指向本源文件打包进宿主产物。
//      ——依赖注入相反向：SDK 零 import（不依赖 vue / 宿主树），可独立编译。
//
// # 三行接入示例
//
// ```ts
// import { createSdk } from '@nexos/app-sdk'          // 构建期重写到宿主桥
// const sdk = ctx.sdk ?? createSdk(ctx.api)           // 宿主已备好实例则直用
// const caps = await sdk.capabilities.get()           // 能力快照（5s 缓存）
// ```
//
// # 桥载体形态（协议版本化 sdk.version='0.1'）
//
// __NEXOS_HOST__.sdk 既是**就绪实例**（宿主 api 注入完毕，应用可直接
// ctx.sdk.capabilities.get()）又携带**工厂面**（sdk.createSdk / sdk.SDK_VERSION
// ——host-externals 的虚拟模块从该对象解构导出，应用 `import { createSdk }`
// 照常编译；需要自定义 opts（getToken/notify/fetch 注入）的应用可自行再建）。
// =============================================================================

import {
    createCapabilitiesApi,
    SDK_VERSION,
    type CapabilitiesApi,
    type HostApi,
} from './capabilities'
import { createDegradedApi, type DegradedApi } from './degraded'
import { createGatewayApi, type GatewayApi } from './gateway'
import { createLlmApi, type LlmApi } from './llm'
import { createLobbyApi, type LobbyApi } from './lobby'
import { createNotifier, type NotifyFn } from './notify'

export { SDK_VERSION } from './capabilities'
export type { CapabilitySnapshot, CapabilitiesApi, HostApi } from './capabilities'
export {
    createDegradedApi,
    degradedOf,
    missingOf,
    offlineState,
    PROBE_RETRIES,
} from './degraded'
export type { DegradedApi, DegradedState } from './degraded'
export {
    createGatewayApi,
    createSseParser,
    chatDeltaFromPayload,
} from './gateway'
export type {
    ChatDelta,
    ChatMessage,
    ChatOpts,
    ChatResult,
    GatewayApi,
    GatewayChannel,
} from './gateway'
export { createLlmApi, llmInstanceId } from './llm'
export type { LlmApi, LlmInstance, LlmInstanceRef } from './llm'
export { createLobbyApi, resolveChannel } from './lobby'
export type { LobbyApi, LobbyEntry } from './lobby'
export { createNotifier } from './notify'
export type { NotifyFn } from './notify'

// =============================================================================
// createSdk
// =============================================================================

/** createSdk 可选项（全部可缺省）。 */
export interface SdkOptions {
    /** sk-os-/admin 令牌读取器（网关 SSE 等裸 fetch 用）。缺省读
     * localStorage 'os-api-token'（与宿主设置页/独立模式底栏同 key）。 */
    getToken?: () => string
    /** 通知后端注入（嵌入模式宿主传主前端 toast）。缺省按 notify.ts 三档策略。 */
    notify?: NotifyFn
    /** fetch 注入（测试 / 代理场景）。缺省 globalThis.fetch。 */
    fetchImpl?: typeof fetch
    /** 能力快照缓存 TTL（毫秒）。缺省 5000。 */
    capabilitiesTtlMs?: number
}

/** NexOS 应用 SDK 实例（createSdk 返回；ctx.sdk 即本形态）。 */
export interface NexosSdk {
    /** SDK 协议版本（'0.1'；宿主桥与应用的兼容锚点）。 */
    version: string
    /** 能力快照面。 */
    capabilities: CapabilitiesApi
    /** 降级三态面（full / degraded / offline + missing）。 */
    degraded: DegradedApi
    /** 联邦大厅面（条目 + 经网关转发对话）。 */
    lobby: LobbyApi
    /** API 网关面（渠道 + OpenAI 兼容 chat，SSE 流式）。 */
    gateway: GatewayApi
    /** 本地推理面（实例 + 直连 chat）。 */
    llm: LlmApi
    /** 通知（嵌入=宿主 toast；独立=系统通知，无权限降级迷你 toast）。 */
    notify(title: string, body?: string): void
    // —— 工厂面（桥载体：__NEXOS_HOST__.sdk 既是实例也是 host-externals
    //    虚拟模块的导出源；应用 `import { createSdk } from '@nexos/app-sdk'`
    //    解构到的就是这里挂的工厂）——
    /** 同模块 createSdk（自定义 api/opts 时自建实例）。 */
    createSdk: typeof createSdk
    /** 同模块 SDK_VERSION。 */
    SDK_VERSION: string
}

/**
 * 创建 NexOS 应用 SDK 实例。
 *
 * @param api 宿主 api 原语（ctx.api——鉴权/超时/错误处理全由宿主承担）
 * @param opts 可选注入（令牌读取/通知后端/fetch/缓存 TTL）
 */
export function createSdk(api: HostApi, opts: SdkOptions = {}): NexosSdk {
    const fetchImpl: typeof fetch =
        opts.fetchImpl ??
        ((...args) => {
            const f = (globalThis as { fetch?: typeof fetch }).fetch
            if (!f) throw new Error('createSdk: 运行环境无 fetch（请经 opts.fetchImpl 注入）')
            return f(...args)
        })
    const getToken = opts.getToken ?? defaultGetToken

    const capabilities = createCapabilitiesApi(api, { ttlMs: opts.capabilitiesTtlMs })
    const gateway = createGatewayApi({ api, fetchImpl, getToken })
    const lobby = createLobbyApi({ api, gateway })
    const llm = createLlmApi(api)
    const notify: NotifyFn = createNotifier({ notify: opts.notify })
    // 降级面探测走 capabilities 缓存口径（state() 消费 5s 热缓存，
    // refresh() 绕过）。
    const degraded = createDegradedApi({ probe: (force) => capabilities.get({ force }) })

    const sdk: NexosSdk = {
        version: SDK_VERSION,
        capabilities,
        degraded,
        lobby,
        gateway,
        llm,
        notify,
        createSdk,
        SDK_VERSION,
    }
    return sdk
}

/** 缺省令牌读取：localStorage 'os-api-token'（与宿主设置页/独立底栏同 key）。 */
export const TOKEN_STORAGE_KEY = 'os-api-token'

function defaultGetToken(): string {
    try {
        return (globalThis as { localStorage?: Storage }).localStorage?.getItem(
            TOKEN_STORAGE_KEY,
        ) ?? ''
    } catch {
        return ''
    }
}

/** 默认导出 = createSdk（无构建宿主 `import createSdk from '@nexos/app-sdk'` 顺手）。 */
export default createSdk
