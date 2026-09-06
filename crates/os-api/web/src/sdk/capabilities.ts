// =============================================================================
// capabilities —— 能力快照（@nexos/app-sdk 的探测面）。
//
// 数据源：GET /api/v1/capabilities（os-api，读公开，秒回——服务端只聚合
// 既有内存态/缓存，零主动探测联邦）。本模块职责：
//   1. 快照类型契约（与 capabilities.rs 的 serde 形状逐字段对齐）；
//   2. 5s 内缓存（get() 命中热缓存不重复请求）+ refresh() 强制刷新；
//   3. 订阅（subscribe）——每次新快照落地时通知（能力热插拔感知面）。
//
// 本文件是 @nexos/app-sdk 的**唯一事实源**（crates/os-api/web/src/sdk/）；
// 应用包经宿主桥 globalThis.__NEXOS_HOST__.sdk 消费（构建期 host-externals
// 重写 import，零打包）。
// =============================================================================

/** SDK 协议版本（与服务端 capabilities.rs 的 SDK_VERSION 同步）。 */
export const SDK_VERSION = '0.1'

/** LLM 本地推理能力面。 */
export interface CapsLlm {
    /** 实例总数（全部状态）。 */
    instances: number
    /** status==running 的实例 id 列表。 */
    running: string[]
}

/** 网关渠道能力面。 */
export interface CapsGateway {
    /** 渠道总数。 */
    channels: number
    /** 启用中的渠道数。 */
    enabled: number
    /** 🌐 联邦中继渠道数（via_node 非空）。 */
    relay_channels: number
}

/** 联邦大厅（API 市场）能力面——服务端纯缓存口径。 */
export interface CapsLobby {
    /** 大厅条目数（本地 + 联邦接收缓存）。 */
    entries: number
    /** 条目心跳缓存里最新的一条（RFC3339；无任何心跳 → null）。 */
    last_sync_at: string | null
    /** 按缓存判定：任一条目心跳新鲜（≤60s）→ true。 */
    reachable: boolean
}

/** 媒体能力面。 */
export interface CapsMedia {
    /** ffmpeg 可用（env NEXOS_FFMPEG_BIN → PATH → 常规路径）。 */
    ffmpeg_available: boolean
}

/** P2P 组网能力面。 */
export interface CapsP2p {
    /** 组网是否启用（NEXOS_P2P_ENABLE；部署缺省关——缺省不算能力缺失）。 */
    enabled: boolean
    /** 已认证直连对端数。 */
    peers_connected: number
}

/** 能力快照（GET /api/v1/capabilities 响应体，冻结线格式）。 */
export interface CapabilitySnapshot {
    /** SDK 协议版本（恒 '0.1'，与服务端同步）。 */
    sdk_version: string
    /** 服务端生成时刻（RFC3339）。 */
    generated_at: string
    llm: CapsLlm
    gateway: CapsGateway
    lobby: CapsLobby
    media: CapsMedia
    p2p: CapsP2p
    /** 已装应用包 id 列表。 */
    apps: string[]
}

/** 宿主 api 原语（与 ctx.api 同签名——get/post/del/request）。 */
export interface HostApi {
    get<T>(path: string): Promise<T>
    post<T>(path: string, body?: unknown): Promise<T>
    del<T>(path: string): Promise<T>
    request<T>(path: string, opts?: { method?: string; body?: unknown }): Promise<T>
}

/** 能力面 API（sdk.capabilities）。 */
export interface CapabilitiesApi {
    /**
     * 取能力快照：5s 内的缓存直接返回（避免应用每个组件各拉一次）；
     * force=true 等价 refresh()。探测失败抛错（降级判定走 degraded 模块）。
     */
    get(opts?: { force?: boolean }): Promise<CapabilitySnapshot>
    /** 强制刷新（绕过缓存），新快照通知订阅者。 */
    refresh(): Promise<CapabilitySnapshot>
    /** 最近一次成功快照（从未成功 → null；不触发网络）。 */
    cached(): CapabilitySnapshot | null
    /** 订阅新快照；返回取消订阅函数。 */
    subscribe(cb: (s: CapabilitySnapshot) => void): () => void
}

/** 缓存 TTL（毫秒）——任务口径 5s。 */
export const CAPS_TTL_MS = 5_000

/** 构造能力面（index.ts 装配用；应用一般不直接调）。 */
export function createCapabilitiesApi(
    api: HostApi,
    opts?: { ttlMs?: number; onSnapshot?: (s: CapabilitySnapshot) => void },
): CapabilitiesApi {
    const ttl = opts?.ttlMs ?? CAPS_TTL_MS
    let cache: CapabilitySnapshot | null = null
    let fetchedAt = 0
    let inflight: Promise<CapabilitySnapshot> | null = null
    const subscribers = new Set<(s: CapabilitySnapshot) => void>()

    async function fetchFresh(): Promise<CapabilitySnapshot> {
        // 并发去重：同一时刻多个调用共享同一次请求。
        if (inflight) return inflight
        inflight = (async () => {
            try {
                const snap = await api.get<CapabilitySnapshot>('/api/v1/capabilities')
                cache = snap
                fetchedAt = Date.now()
                for (const cb of subscribers) {
                    try {
                        cb(snap)
                    } catch {
                        /* 订阅者异常不影响其他订阅者 */
                    }
                }
                opts?.onSnapshot?.(snap)
                return snap
            } finally {
                inflight = null
            }
        })()
        return inflight
    }

    return {
        get(opts2) {
            if (!opts2?.force && cache && Date.now() - fetchedAt < ttl) {
                return Promise.resolve(cache)
            }
            return fetchFresh()
        },
        refresh: () => fetchFresh(),
        cached: () => cache,
        subscribe(cb: (s: CapabilitySnapshot) => void): () => void {
            subscribers.add(cb)
            return () => subscribers.delete(cb)
        },
    }
}
