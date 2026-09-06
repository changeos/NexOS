// =============================================================================
// lobby —— 联邦大厅能力面（@nexos/app-sdk）：API 市场条目 + 经网关转发对话。
//
// 数据端点：GET /api/v1/api-market（读公开；q/sort/scope 参数）——元素带
// 🌐 联邦徽章字段 source_node（local=本机）/ source_node_id（NodeID）与
// heartbeat 缓存（详情端点另有 heartbeat_fresh 派生字段）。
//
// entryRef → 网关渠道映射口径（调研后定，优先级从高到低）：
//   1. entry.channel_id 显式指定（调用方自行管理映射时直接命中）；
//   2. 渠道 name === entry.api_name（联邦条目「一键导入为渠道」的默认命名）；
//   3. 渠道 via_node === entry.source_node_id（同源节点的中继渠道）；
//   4. 渠道 models 包含 entry.server_config.model_name（模型名命中）；
//   全部未命中 → 抛可读错误（引导先在 API 网关把条目导入为渠道）。
//
// 命中后转发经 sdk.gateway.chat（POST /api/v1/gateway/v1/chat/completions，
// sk-os- 鉴权与 SSE 流式同 gateway 模块）。
// =============================================================================

import type { HostApi } from './capabilities'
import type { GatewayApi, ChatMessage, ChatOpts, GatewayChannel } from './gateway'

/** 联邦大厅条目（GET /api/v1/api-market 元素子集，宽松字段）。 */
export interface LobbyEntry {
    id: string
    /** 条目名（一键导入的默认渠道名）。 */
    api_name: string
    description?: string
    endpoint_url?: string
    /** 本机发布恒 'local'；联邦条目 = 发布节点名（🌐 徽章字段）。 */
    source_node?: string
    /** 联邦来源 NodeID（本机发布空串）。 */
    source_node_id?: string
    server_config?: { model_name?: string; [k: string]: unknown } | null
    pricing?: { mode?: string; [k: string]: unknown } | null
    heartbeat_at?: string | null
    /** 应用自管理的显式渠道映射（SDK 消费的最高优先级字段）。 */
    channel_id?: string
    [k: string]: unknown
}

/** 大厅能力面 API（sdk.lobby）。 */
export interface LobbyApi {
    /**
     * 大厅条目列表。opts.scope: all（缺省）/ local（本机）/ fed（联邦远程）。
     */
    list(opts?: { q?: string; scope?: 'all' | 'local' | 'fed' }): Promise<LobbyEntry[]>
    /**
     * 经网关渠道转发对话（entryRef → 渠道映射见文件头；stream/onDelta/onDone/
     * onError 语义同 sdk.gateway.chat）。
     */
    chat(entryRef: LobbyEntry | string, opts: ChatOpts & { messages: ChatMessage[] }): Promise<
        import('./gateway').ChatResult
    >
}

/** 构造大厅面（index.ts 装配用）。 */
export function createLobbyApi(deps: { api: HostApi; gateway: GatewayApi }): LobbyApi {
    async function list(
        opts?: { q?: string; scope?: 'all' | 'local' | 'fed' },
    ): Promise<LobbyEntry[]> {
        const params = new URLSearchParams()
        if (opts?.q) params.set('q', opts.q)
        if (opts?.scope && opts.scope !== 'all') params.set('scope', opts.scope)
        const qs = params.toString()
        const raw = await deps.api.get<unknown>(`/api/v1/api-market${qs ? '?' + qs : ''}`)
        return Array.isArray(raw) ? (raw as LobbyEntry[]) : []
    }

    return {
        list,
        async chat(entryRef, opts) {
            // —— 1. 解析条目（对象直用；字符串 id 经 list 缓存解析）——
            let entry: LobbyEntry | null = null
            if (typeof entryRef === 'object' && entryRef !== null) {
                entry = entryRef
            } else {
                const id = String(entryRef ?? '').trim()
                entry = (await list()).find((e) => e.id === id) ?? null
                if (!entry) throw new Error(`lobby.chat: 大厅条目不存在: ${id}`)
            }
            const modelName = String(entry.server_config?.model_name ?? '').trim()

            // —— 2. entryRef → 渠道映射（优先级见文件头）——
            const channels = await deps.gateway.channels()
            const channel = resolveChannel(entry, channels)
            if (!channel) {
                throw new Error(
                    `lobby.chat: 条目「${entry.api_name || entry.id}」未找到对应网关渠道` +
                        `（可在 API 网关 → 联邦大厅将该条目导入为渠道后重试）`,
                )
            }

            // —— 3. 经网关渠道转发（模型名优先条目声明的上游名）——
            const model = modelName || channel.models?.[0] || entry.api_name
            const { messages, ...rest } = opts
            return deps.gateway.chat(model, messages, { ...rest, model })
        },
    }
}

/**
 * 渠道映射纯函数（单测覆盖四优先级与未命中）。
 */
export function resolveChannel(
    entry: LobbyEntry,
    channels: GatewayChannel[],
): GatewayChannel | null {
    // 1) 显式 channel_id
    if (entry.channel_id) {
        const hit = channels.find((c) => c.id === entry.channel_id)
        if (hit) return hit
    }
    const name = String(entry.api_name ?? '').trim()
    // 2) 渠道名 === 条目名（一键导入命名约定）
    if (name) {
        const byName = channels.find((c) => (c.name ?? '').trim() === name)
        if (byName) return byName
    }
    // 3) 中继渠道 via_node === 条目来源 NodeID
    const src = String(entry.source_node_id ?? '').trim()
    if (src) {
        const byNode = channels.find((c) => (c.via_node ?? '').trim() === src)
        if (byNode) return byNode
    }
    // 4) 渠道模型列表包含条目模型名
    const model = String(entry.server_config?.model_name ?? '').trim()
    if (model) {
        const byModel = channels.find((c) => (c.models ?? []).includes(model))
        if (byModel) return byModel
    }
    return null
}
