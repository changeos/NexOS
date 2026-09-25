// =============================================================================
// gateway —— 网关能力面（@nexos/app-sdk）：渠道列表 + OpenAI 兼容 chat
// （POST /api/v1/gateway/v1/chat/completions，sk-os- 令牌鉴权，流式 SSE）。
//
// SSE 解析器说明（任务口径："先调研主前端既有 SSE 客户端……不能抽就独立
// 实现并注明"）：主前端既有的 SSE 消费是 **视图内联实现**
// （crates/os-api/web/src/views/LlmModels.vue `parseDeltaFields` +
// `mcSend()` 里的 ReadableStream + TextDecoder 逐行解析）——无法在不碰
// 视图的前提下抽出公共模块（禁令：主前端除 appRuntime/sdk/client 外不碰
// 视图）。故本文件给出 SDK 内独立实现，语义与视图版对齐并增强：
//   - 逐行切分保留不完整尾行（跨 chunk 安全）；
//   - `:` 注释行忽略（网关中途断流会补 `: gateway: …` 注释帧）；
//   - `data: [DONE]` 终止；delta 双键兼容（vLLM 0.28 `reasoning` /
//     0.27 `reasoning_content`）。
//
// 鉴权：sk-os- 令牌从宿主同一存储取（localStorage 'os-api-token'——桌面
// 「设置 → API 令牌」与独立模式 token 底栏同 key 同源）；SSE 需要流式
// response body，宿主 api.request() 原语（内部 .text() 收整包）不适用，
// 故本模块直接用 fetch（可经 opts.fetchImpl 注入，测试用）。
// =============================================================================

import type { HostApi } from './capabilities'

/** OpenAI 兼容 chat 消息。 */
export interface ChatMessage {
    role: 'system' | 'user' | 'assistant' | string
    content: string
}

/** 网关渠道（GET /api/v1/gateway/channels 元素子集，宽松字段）。 */
export interface GatewayChannel {
    id: string
    name: string
    provider?: string
    /** 该渠道支持的模型列表。 */
    models?: string[]
    enabled?: boolean
    /** enabled / disabled / error。 */
    status?: string
    /** 🌐 联邦中继来源 NodeID（非空 = 中继渠道）。 */
    via_node?: string
    [k: string]: unknown
}

/** chat 增量（正文 + 思考段双键兼容）。 */
export interface ChatDelta {
    content: string
    reasoning: string
}

/** 一次 chat 的最终结果（流式为全程聚合；非流式取 message）。 */
export interface ChatResult {
    content: string
    reasoning: string
    finish_reason?: string | null
    usage?: { total_tokens?: number } | null
}

/** 流式回调 + 参数（sdk.gateway.chat / sdk.lobby.chat 共用）。 */
export interface ChatOpts {
    /** true=强制流式；缺省=提供 onDelta 时流式，否则整包。 */
    stream?: boolean
    max_tokens?: number
    temperature?: number
    /** 流式增量回调。 */
    onDelta?: (d: ChatDelta) => void
    /** 流正常结束（含 onDone 前的全部聚合结果）。 */
    onDone?: (r: ChatResult) => void
    /** 失败回调（Promise 同时 reject——回调是通知，不是替代 catch）。 */
    onError?: (e: Error) => void
    /** 取消信号（fetch AbortSignal）。 */
    signal?: AbortSignal
    /** 覆盖模型名（lobby 转发时用上游真实名）。 */
    model?: string
    /** 覆盖鉴权令牌（缺省走宿主存储）。 */
    token?: string
}

/** SSE 事件接收器。 */
export interface SseSink {
    /** 每个 data: 帧的原文（已剥 `data:` 前缀与 \r；[DONE] 不会到达这里）。 */
    onData(payload: string): void
    /** [DONE] 帧或流结束（消费方可省略——end() 后仍可读聚合结果）。 */
    onDone?(): void
}

/**
 * 增量 SSE 解析器（分段正确性：任意 chunk 边界安全——尾行缓存到下一段）。
 * 用法：`const p = createSseParser(sink); p.push(text); … p.end()`。
 */
export function createSseParser(sink: SseSink): {
    push(chunk: string): void
    end(): void
} {
    let buffer = ''
    let done = false
    function handleLine(raw: string): void {
        const line = raw.endsWith('\r') ? raw.slice(0, -1) : raw
        if (line === '') return // 事件边界（OpenAI 单行 data，无需聚合）
        if (line.startsWith(':')) return // SSE 注释（网关断流说明帧等）
        if (!line.startsWith('data:')) return // event:/id:/retry: 忽略
        const payload = line.slice(5).trimStart()
        if (payload === '[DONE]') {
            done = true
            sink.onDone?.()
            return
        }
        if (payload) sink.onData(payload)
    }
    return {
        push(chunk: string): void {
            if (done) return
            buffer += chunk
            const lines = buffer.split('\n')
            buffer = lines.pop() ?? '' // 最后一段可能不完整——留到下一 chunk
            for (const line of lines) handleLine(line)
        },
        end(): void {
            if (buffer) handleLine(buffer)
            buffer = ''
            if (!done) {
                done = true
                sink.onDone?.()
            }
        },
    }
}

/**
 * 单个 data: 帧原文 → chat 增量（非 JSON / 无 delta 返回空增量——解析器
 * 不抛错，与主前端视图版同语义）。
 */
export function chatDeltaFromPayload(payload: string): ChatDelta {
    try {
        const obj = JSON.parse(payload) as {
            choices?: Array<{
                delta?: {
                    content?: string
                    reasoning?: string
                    reasoning_content?: string
                }
            }>;
        }
        const delta = obj.choices?.[0]?.delta
        if (!delta) return { content: '', reasoning: '' }
        return {
            content: typeof delta.content === 'string' ? delta.content : '',
            reasoning:
                (typeof delta.reasoning === 'string' && delta.reasoning) ||
                (typeof delta.reasoning_content === 'string'
                    ? delta.reasoning_content
                    : ''),
        }
    } catch {
        return { content: '', reasoning: '' }
    }
}

/** 网关能力面 API（sdk.gateway）。 */
export interface GatewayApi {
    /** 渠道列表（GET /api/v1/gateway/channels，经宿主 api 原语）。 */
    channels(): Promise<GatewayChannel[]>
    /**
     * OpenAI 兼容对话（POST /api/v1/gateway/v1/chat/completions）。
     * - stream（缺省=有 onDelta 即流式）：SSE 逐帧 onDelta → onDone(聚合)；
     * - 非流式：整包 JSON → resolve ChatResult；
     * - 失败：onError 通知 + Promise reject（无 onDelta 的 await 用法照常 catch）。
     */
    chat(
        model: string,
        messages: ChatMessage[],
        opts?: ChatOpts,
    ): Promise<ChatResult>
}

/** 构造网关面（index.ts 装配用）。 */
export function createGatewayApi(deps: {
    api: HostApi
    fetchImpl: typeof fetch
    getToken: () => string
}): GatewayApi {
    /** 统一错误（带 HTTP 状态与响应体摘要）。 */
    async function errorFromResp(resp: Response): Promise<Error> {
        let detail = ''
        try {
            const text = await resp.text()
            try {
                const j = JSON.parse(text) as { error?: { message?: string }; message?: string }
                detail =
                    (j as { error?: { message?: string } }).error?.message ??
                    (j as { message?: string }).message ??
                    text.slice(0, 200)
            } catch {
                detail = text.slice(0, 200)
            }
        } catch {
            /* ignore */
        }
        return new Error(`网关请求失败（HTTP ${resp.status}）${detail ? ': ' + detail : ''}`)
    }

    function authHeaders(opts?: ChatOpts): Record<string, string> {
        const token = opts?.token ?? deps.getToken()
        return token.trim()
            ? { Authorization: `Bearer ${token.trim()}` }
            : {}
    }

    return {
        channels() {
            return deps.api.get<GatewayChannel[]>('/api/v1/gateway/channels')
        },
        async chat(model, messages, opts = {}) {
            const useStream = opts.stream ?? typeof opts.onDelta === 'function'
            const body: Record<string, unknown> = { model, messages }
            if (useStream) body.stream = true
            if (opts.max_tokens !== undefined) body.max_tokens = opts.max_tokens
            if (opts.temperature !== undefined) body.temperature = opts.temperature
            const run = deps.fetchImpl('/api/v1/gateway/v1/chat/completions', {
                method: 'POST',
                headers: {
                    'Content-Type': 'application/json',
                    Accept: useStream ? 'text/event-stream' : 'application/json',
                    ...authHeaders(opts),
                },
                body: JSON.stringify(body),
                signal: opts.signal,
            })

            // —— 非流式：整包 JSON ——
            if (!useStream) {
                try {
                    const resp = await run
                    if (!resp.ok) throw await errorFromResp(resp)
                    const v = (await resp.json()) as {
                        choices?: Array<{
                            message?: {
                                content?: string
                                reasoning?: string
                                reasoning_content?: string
                            }
                            finish_reason?: string
                        }>;
                        usage?: { total_tokens?: number };
                    }
                    const msg = v.choices?.[0]?.message
                    const result: ChatResult = {
                        content: msg?.content ?? '',
                        reasoning: msg?.reasoning ?? msg?.reasoning_content ?? '',
                        finish_reason: v.choices?.[0]?.finish_reason ?? null,
                        usage: v.usage ?? null,
                    }
                    opts.onDone?.(result)
                    return result
                } catch (e) {
                    const err = e instanceof Error ? e : new Error(String(e))
                    opts.onError?.(err)
                    throw err
                }
            }

            // —— 流式：ReadableStream + SSE 解析器（回调 + 聚合双轨）——
            try {
                const resp = await run
                if (!resp.ok || !resp.body) throw await errorFromResp(resp)
                const reader = resp.body.getReader()
                const decoder = new TextDecoder('utf-8')
                const result: ChatResult = { content: '', reasoning: '', usage: null }
                const parser = createSseParser({
                    onData(payload) {
                        const d = chatDeltaFromPayload(payload)
                        if (!d.content && !d.reasoning) return
                        result.content += d.content
                        result.reasoning += d.reasoning
                        opts.onDelta?.(d)
                    },
                })
                for (;;) {
                    const { done, value } = await reader.read()
                    if (done) break
                    parser.push(decoder.decode(value, { stream: true }))
                }
                parser.end()
                opts.onDone?.(result)
                return result
            } catch (e) {
                const err = e instanceof Error ? e : new Error(String(e))
                opts.onError?.(err)
                throw err
            }
        },
    }
}
