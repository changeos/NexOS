// =============================================================================
// degraded —— 降级三态（@nexos/app-sdk 的能力判定面）。
//
//   full      —— 全能力（missing 空）
//   degraded  —— 部分能力受限（missing 非空）——应用按 missing 置灰对应入口
//   offline   —— capabilities 探测连败 PROBE_RETRIES 次后判离线
//                （missing=['capabilities']——快照本身拿不到）
//
// missing 键语义（对 CapabilitySnapshot 的派生，纯函数 missingOf）：
//   'llm'          running 实例数为 0（本地分镜/本地 chat 不可用）
//   'gateway'      启用渠道数为 0（渠道转发类：生图/视频/配音/BGM 全不可用）
//   'lobby'        大厅条目数为 0（联邦发现不可用——不代表节点异常）
//   'media.ffmpeg' ffmpeg 不可用（成片合成不可用）
//   注：p2p 未启用是部署缺省（NEXOS_P2P_ENABLE 默认关）而非能力缺失——
//   快照仍透出 p2p 字段，应用可自行判断，但不计入 missing。
//
// 本文件是 @nexos/app-sdk 的唯一事实源（crates/os-api/web/src/sdk/）。
// =============================================================================

import type { CapabilitySnapshot } from './capabilities'

/** 降级模式三态。 */
export type DegradedMode = 'full' | 'degraded' | 'offline'

/** 降级状态（应用 UI 据此显徽章/置灰）。 */
export interface DegradedState {
    mode: DegradedMode
    /** 缺失能力键列表（offline 时恒含 'capabilities'）。 */
    missing: string[]
    /** 本次判定基于的快照 generated_at（offline 时为 null）。 */
    basedOn: string | null
}

/** capabilities 探测重试次数（连败该次数后判 offline）。 */
export const PROBE_RETRIES = 3
/** 重试间隔（毫秒；线性 400ms×n——总计秒级，不拖慢应用首屏）。 */
export const PROBE_RETRY_DELAY_MS = 400

/**
 * 从快照派生缺失能力键（纯函数，单测主战场）。
 * p2p 不计入（部署缺省关，见文件头）。
 */
export function missingOf(snap: CapabilitySnapshot): string[] {
    const missing: string[] = []
    if (!snap.llm || snap.llm.running.length === 0) missing.push('llm')
    if (!snap.gateway || snap.gateway.enabled === 0) missing.push('gateway')
    if (!snap.lobby || snap.lobby.entries === 0) missing.push('lobby')
    if (!snap.media || !snap.media.ffmpeg_available) missing.push('media.ffmpeg')
    return missing
}

/** 快照 → 降级状态（纯函数）。 */
export function degradedOf(snap: CapabilitySnapshot): DegradedState {
    const missing = missingOf(snap)
    return {
        mode: missing.length === 0 ? 'full' : 'degraded',
        missing,
        basedOn: snap.generated_at ?? null,
    }
}

/** offline 常量态（探测连败后）。 */
export function offlineState(): DegradedState {
    return { mode: 'offline', missing: ['capabilities'], basedOn: null }
}

/** 降级面 API（sdk.degraded）。 */
export interface DegradedApi {
    /**
     * 当前降级状态：优先消费 capabilities 的 5s 热缓存；无缓存/过期则探测
     * （失败自动重试至 PROBE_RETRIES 次后判 offline，不再抛错——降级面
     * 的契约是"永远给状态"）。
     */
    state(): Promise<DegradedState>
    /** 强制重新探测并判定。 */
    refresh(): Promise<DegradedState>
    /** 订阅状态变化（capabilities 订阅的派生）；返回取消函数。 */
    subscribe(cb: (s: DegradedState) => void): () => void
}

/** 带重试的探测（index.ts 装配用；应用一般不直接调）。 */
export function createDegradedApi(deps: {
    probe: (force: boolean) => Promise<CapabilitySnapshot>
}): DegradedApi {
    let last: DegradedState | null = null
    const subscribers = new Set<(s: DegradedState) => void>()

    function publish(s: DegradedState): DegradedState {
        const prev = last
        last = s
        // 状态未变化不重复通知（state() 高频调用时避免订阅者刷屏）。
        const unchanged =
            prev &&
            prev.mode === s.mode &&
            prev.missing.length === s.missing.length &&
            prev.missing.every((m, i) => m === s.missing[i])
        if (unchanged) return s
        for (const cb of subscribers) {
            try {
                cb(s)
            } catch {
                /* 订阅者异常不影响其他订阅者 */
            }
        }
        return s
    }

    async function probeWithRetry(force: boolean): Promise<DegradedState> {
        let lastErr: unknown = null
        for (let attempt = 1; attempt <= PROBE_RETRIES; attempt++) {
            try {
                const snap = await deps.probe(force)
                return publish(degradedOf(snap))
            } catch (e) {
                lastErr = e
                if (attempt < PROBE_RETRIES) {
                    await new Promise((r) => setTimeout(r, PROBE_RETRY_DELAY_MS * attempt))
                }
            }
        }
        void lastErr // 探测失败的根因在应用需要时自行 catch capabilities.get()
        return publish(offlineState())
    }

    return {
        state() {
            // 探测入口统一走重试包装（state 与 refresh 语义差异仅在是否绕过缓存——
            // 由 deps.probe 的 force 参数表达）。
            return probeWithRetry(false)
        },
        refresh() {
            return probeWithRetry(true)
        },
        subscribe(cb: (s: DegradedState) => void): () => void {
            subscribers.add(cb)
            // 已有判定先回调一次（订阅即得当前态，免竞态等待）。
            if (last) {
                try {
                    cb(last)
                } catch {
                    /* ignore */
                }
            }
            return () => subscribers.delete(cb)
        },
    }
}
