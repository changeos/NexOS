// =============================================================================
// llm —— 本地推理能力面（@nexos/app-sdk）：实例列表 + 本地实例直连 chat。
//
// chat 语义复用 film 的「local chat」口径（docs/APPS.md / film.rs）：
// 服务端 LlmRouteHandler::chat_complete 直连 127.0.0.1:<port>/v1/chat/
// completions（vLLM OpenAI 兼容面），应用侧走 REST 封装
// POST /api/v1/llm/instances/:id/chat（鉴权同宿主 api——开发期无凭据注入
// 默认 admin，独立模式 401/403 式 token 底栏已有）。reasoning 双键由服务端
// 归一为 `reasoning`。
// =============================================================================

import type { HostApi } from './capabilities'
import type { ChatMessage } from './gateway'

/** 本地 LLM 实例（GET /api/v1/llm/instances 元素子集，宽松字段）。 */
export interface LlmInstance {
    id: string
    name?: string
    /** 模型名或路径。 */
    model?: string
    /** stopped / starting / running / error。 */
    status?: string
    port?: number
    config?: { served_model_name?: string | null } | null
    [k: string]: unknown
}

/** 本地实例 chat 结果（服务端 ChatOutcome 形状）。 */
export interface LlmChatResult {
    content: string
    /** 思考段（无则空串）。 */
    reasoning: string
    finish_reason?: string | null
    total_tokens?: number | null
}

/** 实例引用：id 字符串 / 数字 / 实例对象。 */
export type LlmInstanceRef = string | number | LlmInstance

/** chat 可选参数。 */
export interface LlmChatOpts {
    max_tokens?: number
    temperature?: number
    /** vLLM chat template 关键字透传（如 {enable_thinking:false}）。 */
    chat_template_kwargs?: Record<string, unknown>
}

/** 本地推理能力面 API（sdk.llm）。 */
export interface LlmApi {
    /** 实例列表（GET /api/v1/llm/instances）。 */
    instances(): Promise<LlmInstance[]>
    /** running 实例子集（本地 chat 前置条件）。 */
    running(): Promise<LlmInstance[]>
    /**
     * 本地实例直连对话（POST /api/v1/llm/instances/:id/chat，同步整包）。
     * instanceRef 解析失败（空 id）抛错。
     */
    chat(
        instanceRef: LlmInstanceRef,
        messages: ChatMessage[],
        opts?: LlmChatOpts,
    ): Promise<LlmChatResult>
}

/** 实例引用 → id（对象取 id；空值抛可读错误）。 */
export function llmInstanceId(ref: LlmInstanceRef): string {
    const id = typeof ref === 'object' && ref !== null ? ref.id : ref
    const s = id === undefined || id === null ? '' : String(id).trim()
    if (!s) throw new Error('llm.chat: 实例引用缺少 id（传实例 id 或列表元素）')
    return s
}

/** 构造本地推理面（index.ts 装配用）。 */
export function createLlmApi(api: HostApi): LlmApi {
    async function instances(): Promise<LlmInstance[]> {
        const raw = await api.get<unknown>('/api/v1/llm/instances')
        return Array.isArray(raw) ? (raw as LlmInstance[]) : []
    }
    return {
        instances,
        async running() {
            return (await instances()).filter((i) => (i.status ?? '') === 'running')
        },
        async chat(instanceRef, messages, opts = {}) {
            const id = llmInstanceId(instanceRef)
            const body: Record<string, unknown> = { messages }
            if (opts.max_tokens !== undefined) body.max_tokens = opts.max_tokens
            if (opts.temperature !== undefined) body.temperature = opts.temperature
            if (opts.chat_template_kwargs !== undefined) {
                body.chat_template_kwargs = opts.chat_template_kwargs
            }
            return api.post<LlmChatResult>(
                `/api/v1/llm/instances/${encodeURIComponent(id)}/chat`,
                body,
            )
        },
    }
}
