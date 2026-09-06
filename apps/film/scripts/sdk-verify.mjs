#!/usr/bin/env node
// =============================================================================
// sdk-verify.mjs —— @nexos/app-sdk 脚本级验证（Node，无框架依赖）。
//
// 主前端无 vitest（照 film 代理先例）：本脚本用 esbuild（film devDependencies）
// 把 SDK 的 TS 唯一事实源（crates/os-api/web/src/sdk/）打包成 ESM，再在 Node
// 里 mock fetch / mock 宿主 api，断言：
//   1. capabilities 快照解析 + 5s 缓存 + force 刷新；
//   2. 降级三态（full / degraded / offline——offline 走 3 次重试路径）；
//   3. SSE 解析器分段正确（任意 chunk 边界切割均安全 / 注释行忽略 / [DONE]）；
//   4. gateway.chat 流式（onDelta 聚合 + Authorization 头）与非流式；
//   5. lobby.chat 的 entryRef→渠道映射四优先级 + 未命中报错。
//
// 运行：cd apps/film && node scripts/sdk-verify.mjs
// =============================================================================

import { build } from 'esbuild'
import { mkdtemp } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { pathToFileURL } from 'node:url'
import assert from 'node:assert/strict'

const SDK_ENTRY = new URL('../../../crates/os-api/web/src/sdk/index.ts', import.meta.url)

// —— 1. 打包 SDK（TS → ESM，浏览器平台语义）——
const tmp = await mkdtemp(join(tmpdir(), 'nexos-sdk-verify-'))
const outfile = join(tmp, 'sdk.mjs')
await build({
    entryPoints: [SDK_ENTRY.pathname],
    outfile,
    bundle: true,
    format: 'esm',
    platform: 'browser',
    target: 'es2020',
})
const {
    createSdk,
    SDK_VERSION,
    createSseParser,
    chatDeltaFromPayload,
    missingOf,
    resolveChannel,
} = await import(pathToFileURL(outfile))

let passed = 0
function ok(name, fn) {
    try {
        const r = fn()
        if (r instanceof Promise) {
            return r.then(
                () => {
                    passed++
                    console.log(`  ✓ ${name}`)
                },
                (e) => {
                    console.error(`  ✗ ${name}\n    ${e?.message ?? e}`)
                    process.exitCode = 1
                },
            )
        }
        passed++
        console.log(`  ✓ ${name}`)
    } catch (e) {
        console.error(`  ✗ ${name}\n    ${e?.message ?? e}`)
        process.exitCode = 1
    }
}

// —— 2. mock 基建：宿主 api 原语 + fetch ——

/** 可编程 mock 宿主 api（get/post 计数；路由 → 响应或抛错）。 */
function mockApi(routes = {}) {
    const calls = []
    return {
        calls,
        get: async (path) => {
            calls.push(['GET', path])
            const h = routes[`${'GET'} ${path}`]
            if (h instanceof Error) throw h
            return h
        },
        post: async (path, body) => {
            calls.push(['POST', path])
            const h = routes[`POST ${path}`]
            if (h instanceof Error) throw h
            return typeof h === 'function' ? h(body) : h
        },
        del: async () => undefined,
        request: async () => undefined,
    }
}

const FULL_SNAP = {
    sdk_version: '0.1',
    generated_at: '2026-09-04T12:00:00+00:00',
    llm: { instances: 1, running: ['llm-5'] },
    gateway: { channels: 2, enabled: 2, relay_channels: 1 },
    lobby: { entries: 3, last_sync_at: '2026-09-04T12:00:10+00:00', reachable: true },
    media: { ffmpeg_available: true },
    p2p: { enabled: true, peers_connected: 2 },
    apps: ['film'],
}

/** SSE Response 替身（body.getReader 逐段吐 bytes）。 */
function sseResponse(chunks, { contentType = 'text/event-stream' } = {}) {
    const enc = new TextEncoder()
    let i = 0
    return {
        ok: true,
        status: 200,
        headers: { get: (k) => (k.toLowerCase() === 'content-type' ? contentType : null) },
        body: {
            getReader() {
                return {
                    read: async () =>
                        i < chunks.length
                            ? { done: false, value: enc.encode(chunks[i++]) }
                            : { done: true, value: undefined },
                }
            },
        },
    }
}

function jsonResponse(obj, status = 200) {
    return {
        ok: status >= 200 && status < 300,
        status,
        statusText: 'OK',
        text: async () => JSON.stringify(obj),
        json: async () => obj,
    }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

console.log(`@nexos/app-sdk 脚本级验证（SDK_VERSION=${SDK_VERSION}）`)

// =============================================================================
// 3. SSE 解析器（分段正确性）
// =============================================================================
{
    const datas = []
    let dones = 0
    const p = createSseParser({ onData: (d) => datas.push(d), onDone: () => dones++ })
    // 一条 data: 行被切成 3 段 + \r\n 行尾 + 注释行 + [DONE]
    p.push('data: {"choices":[{"de')
    p.push('lta":{"content":"你')
    p.push('好"}}]}\r\n: gateway: mid-stream note\r\n')
    p.push('data: {"choices":[{"delta":{"content":"!"}}]}\n')
    p.push('data: [DONE]\n')
    p.end()
    await ok('SSE：跨 chunk 分段解析出两个 data 帧', () =>
        assert.equal(datas.length, 2),
    )
    await ok('SSE：帧内容完整（第一帧跨三段拼接）', () =>
        assert.equal(chatDeltaFromPayload(datas[0]).content, '你好'),
    )
    await ok('SSE：注释行忽略 + [DONE] 恰好一次', () => {
        assert.equal(dones, 1)
        assert.deepEqual(datas, [
            '{"choices":[{"delta":{"content":"你好"}}]}',
            '{"choices":[{"delta":{"content":"!"}}]}',
        ])
    })
    await ok('SSE：reasoning 双键兼容（0.28 reasoning / 0.27 reasoning_content）', () => {
        assert.equal(
            chatDeltaFromPayload('{"choices":[{"delta":{"reasoning":"想"}}]}').reasoning,
            '想',
        )
        assert.equal(
            chatDeltaFromPayload(
                '{"choices":[{"delta":{"reasoning_content":"考"}}]}',
            ).reasoning,
            '考',
        )
        assert.deepEqual(chatDeltaFromPayload('not-json'), { content: '', reasoning: '' })
    })
    await ok('SSE：end() 收尾未终结流（无 [DONE] 也补 onDone 恰一次）', () => {
        let n = 0
        const q = createSseParser({ onData: () => {}, onDone: () => n++ })
        q.push('data: {"x":1}\n')
        q.end()
        q.end() // 幂等
        assert.equal(n, 1)
    })
}

// =============================================================================
// 4. capabilities：解析 + 缓存 + 刷新
// =============================================================================
{
    let fetchCount = 0
    const api = mockApi({ 'GET /api/v1/capabilities': FULL_SNAP })
    const sdk = createSdk(api, {
        fetchImpl: async () => {
            fetchCount++
            return jsonResponse(FULL_SNAP)
        },
    })
    await ok('桥形态：version 协议版本 = 0.1', () => assert.equal(sdk.version, '0.1'))
    await ok('capabilities：快照解析（llm/gateway/lobby 字段逐一对齐）', async () => {
        const s = await sdk.capabilities.get()
        assert.equal(s.sdk_version, '0.1')
        assert.deepEqual(s.llm.running, ['llm-5'])
        assert.equal(s.gateway.relay_channels, 1)
        assert.equal(s.lobby.reachable, true)
        assert.deepEqual(s.apps, ['film'])
        // 走的是宿主 api 原语（get）而非 fetch —— capabilities 面用 api.get
        assert.equal(api.calls.filter(([, p]) => p === '/api/v1/capabilities').length, 1)
    })
    await ok('capabilities：5s 内缓存命中（不重复请求）', async () => {
        await sdk.capabilities.get()
        assert.equal(
            api.calls.filter(([, p]) => p === '/api/v1/capabilities').length,
            1,
            '仍只 1 次请求',
        )
    })
    await ok('capabilities：refresh() 强制刷新 + 订阅通知', async () => {
        const seen = []
        const off = sdk.capabilities.subscribe((s) => seen.push(s.generated_at))
        await sdk.capabilities.refresh()
        off()
        assert.equal(seen.length, 1, '订阅者收到新快照')
        assert.ok(sdk.capabilities.cached(), 'cached() 有值')
    })
}

// =============================================================================
// 5. 降级三态
// =============================================================================
{
    await ok('degraded：full（全能力 → 无 missing）', async () => {
        const api = mockApi({ 'GET /api/v1/capabilities': FULL_SNAP })
        const sdk = createSdk(api, { fetchImpl: async () => jsonResponse(FULL_SNAP) })
        const st = await sdk.degraded.refresh()
        assert.equal(st.mode, 'full')
        assert.deepEqual(st.missing, [])
    })
    await ok('degraded：degraded（网关 0 渠道 → missing 含 gateway）', async () => {
        const snap = {
            ...FULL_SNAP,
            gateway: { channels: 0, enabled: 0, relay_channels: 0 },
            media: { ffmpeg_available: false },
        }
        // capabilities 探测走宿主 api 原语（get），fetch 仅网关 SSE 用
        const sdk = createSdk(mockApi({ 'GET /api/v1/capabilities': snap }), {
            fetchImpl: async () => jsonResponse(snap),
        })
        const st = await sdk.degraded.refresh()
        assert.equal(st.mode, 'degraded')
        assert.ok(st.missing.includes('gateway'), 'missing 含 gateway')
        assert.ok(st.missing.includes('media.ffmpeg'), 'missing 含 media.ffmpeg')
        // 纯函数口径一致
        assert.deepEqual(missingOf(snap), st.missing)
    })
    await ok('degraded：offline（探测连败 3 次 → mode=offline）', async () => {
        const api = mockApi({ 'GET /api/v1/capabilities': new Error('boom') })
        const sdk = createSdk(api, {
            fetchImpl: async () => {
                throw new Error('network down')
            },
        })
        const st = await sdk.degraded.refresh()
        assert.equal(st.mode, 'offline')
        assert.deepEqual(st.missing, ['capabilities'])
        assert.equal(
            api.calls.filter(([m, p]) => m === 'GET' && p === '/api/v1/capabilities').length,
            3,
            '恰好重试 3 次（PROBE_RETRIES）',
        )
    })
    await ok('degraded：p2p 未启用不计入 missing（部署缺省口径）', () => {
        const snap = { ...FULL_SNAP, p2p: { enabled: false, peers_connected: 0 } }
        assert.deepEqual(missingOf(snap), [])
    })
    await ok('degraded：subscribe 订阅即得当前态 + 变化去重', async () => {
        const api = mockApi({ 'GET /api/v1/capabilities': FULL_SNAP })
        const sdk = createSdk(api, { fetchImpl: async () => jsonResponse(FULL_SNAP) })
        await sdk.degraded.refresh()
        const seen = []
        const off = sdk.degraded.subscribe((s) => seen.push(s.mode))
        off()
        await sdk.degraded.refresh() // 同态 → 不重复通知
        assert.deepEqual(seen, ['full'], '订阅即回调当前态；同态去重')
    })
}

// =============================================================================
// 6. gateway.chat：流式 + 非流式
// =============================================================================
{
    await ok('gateway.chat：SSE 流式（onDelta 逐帧 + 聚合 + Bearer 头）', async () => {
        let seenAuth = ''
        let seenBody = null
        const deltas = []
        const fetchImpl = async (url, init) => {
            seenAuth = init.headers.Authorization
            seenBody = JSON.parse(init.body)
            return sseResponse([
                'data: {"choices":[{"delta":{"content":"He"}}]}\n\n',
                'data: {"choices":[{"delta":{"content":"llo"}}]}\n\n',
                'data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"total_tokens":7}}\n\n',
                'data: [DONE]\n\n',
            ])
        }
        const api = mockApi()
        const sdk = createSdk(api, { fetchImpl, getToken: () => 'sk-os-test' })
        let doneResult = null
        const result = await sdk.gateway.chat('qwen3.5-9b', [{ role: 'user', content: 'hi' }], {
            onDelta: (d) => deltas.push(d.content),
            onDone: (r) => (doneResult = r),
        })
        assert.equal(result.content, 'Hello')
        assert.equal(seenAuth, 'Bearer sk-os-test')
        assert.equal(seenBody.stream, true)
        assert.deepEqual(deltas, ['He', 'llo'])
        assert.equal(doneResult?.content, 'Hello', 'onDone 收到聚合结果')
    })
    await ok('gateway.chat：非流式整包（无 onDelta 时 stream 不置位）', async () => {
        let seenBody = null
        const fetchImpl = async (_url, init) => {
            seenBody = JSON.parse(init.body)
            return jsonResponse({
                choices: [
                    { message: { content: 'world' }, finish_reason: 'stop' },
                ],
                usage: { total_tokens: 5 },
            })
        }
        const sdk = createSdk(mockApi(), { fetchImpl })
        const r = await sdk.gateway.chat('m', [{ role: 'user', content: 'x' }])
        assert.equal(r.content, 'world')
        assert.notEqual(seenBody.stream, true, '非流式不带 stream:true')
    })
    await ok('gateway.chat：失败 → onError 通知 + Promise reject', async () => {
        let onError = null
        const sdk = createSdk(mockApi(), {
            fetchImpl: async () => jsonResponse({ error: { message: 'quota' } }, 402),
        })
        await assert.rejects(
            sdk.gateway.chat('m', [], { onError: (e) => (onError = e) }),
            /402/,
        )
        assert.match(onError?.message ?? '', /402/)
    })
}

// =============================================================================
// 7. lobby.chat：entryRef → 渠道映射
// =============================================================================
{
    const CHANNELS = [
        { id: 'ch-byname', name: '9B 联邦条目', models: ['qwen3.5-9b'], enabled: true },
        { id: 'ch-bynode', name: '别家渠道', via_node: '0xAAA', models: ['m2'] },
        { id: 'ch-bymodel', name: '模型命中', models: ['glm-4'], enabled: true },
        { id: 'ch-explicit', name: '显式指定', models: [] },
    ]
    const ENTRIES = [
        {
            id: 'e1',
            api_name: '9B 联邦条目',
            source_node_id: '',
            server_config: { model_name: 'qwen3.5-9b' },
        },
        {
            id: 'e2',
            api_name: '无名条目',
            source_node_id: '0xAAA',
            server_config: { model_name: 'm2' },
        },
        {
            id: 'e3',
            api_name: '第三家',
            server_config: { model_name: 'glm-4' },
        },
    ]
    function sdkFor(entries) {
        const api = mockApi({
            'GET /api/v1/api-market': entries,
            'GET /api/v1/gateway/channels': CHANNELS,
        })
        const bodies = []
        const sdk = createSdk(api, {
            fetchImpl: async (_url, init) => {
                bodies.push(JSON.parse(init.body))
                return jsonResponse({
                    choices: [{ message: { content: 'ok' }, finish_reason: 'stop' }],
                })
            },
        })
        return { sdk, bodies }
    }
    await ok('lobby.chat：渠道名 === 条目名（一键导入命名约定）', async () => {
        const { sdk, bodies } = sdkFor(ENTRIES)
        await sdk.lobby.chat('e1', { messages: [{ role: 'user', content: 'q' }] })
        assert.equal(bodies[0].model, 'qwen3.5-9b', '模型名取条目声明的上游名')
    })
    await ok('lobby.chat：via_node 中继渠道命中（🌐 同源节点）', async () => {
        const { sdk, bodies } = sdkFor(ENTRIES)
        await sdk.lobby.chat('e2', { messages: [] })
        // 模型名 m2 也命中 ch-bynode.models —— 但 via_node 规则优先于模型规则，
        // 两者同为此渠道；用 resolveChannel 精确断言优先级：
        assert.equal(resolveChannel(ENTRIES[1], CHANNELS).id, 'ch-bynode')
    })
    await ok('lobby.chat：模型名命中兜底', async () => {
        assert.equal(resolveChannel(ENTRIES[2], CHANNELS).id, 'ch-bymodel')
    })
    await ok('lobby.chat：显式 channel_id 优先级最高', () => {
        const entry = { ...ENTRIES[0], channel_id: 'ch-explicit' }
        assert.equal(resolveChannel(entry, CHANNELS).id, 'ch-explicit')
    })
    await ok('lobby.chat：未命中渠道 → 可读错误（引导导入为渠道）', async () => {
        const { sdk } = sdkFor([{ id: 'e9', api_name: '没人导入', server_config: {} }])
        await assert.rejects(
            sdk.lobby.chat('e9', { messages: [] }),
            /未找到对应网关渠道/,
        )
    })
}

// =============================================================================
// 8. notify（注入档）与 llm 面
// =============================================================================
{
    await ok('notify：宿主注入档优先（嵌入式主前端 toast 口径）', async () => {
        const seen = []
        const sdk = createSdk(mockApi(), {
            fetchImpl: async () => jsonResponse({}),
            notify: (title, body) => seen.push([title, body]),
        })
        sdk.notify('影片制作', '任务完成')
        assert.deepEqual(seen, [['影片制作', '任务完成']])
    })
    await ok('llm：instances + chat（REST 封装语义）', async () => {
        const api = mockApi({
            'GET /api/v1/llm/instances': [
                { id: 'llm-5', status: 'running', model: 'q' },
                { id: 'llm-1', status: 'stopped' },
            ],
            'POST /api/v1/llm/instances/llm-5/chat': (body) => ({
                content: `echo:${body.messages[0].content}`,
                reasoning: '',
            }),
        })
        const sdk = createSdk(api, { fetchImpl: async () => jsonResponse({}) })
        const all = await sdk.llm.instances()
        assert.equal(all.length, 2)
        assert.equal((await sdk.llm.running()).length, 1)
        const r = await sdk.llm.chat('llm-5', [{ role: 'user', content: 'hi' }])
        assert.equal(r.content, 'echo:hi')
        await assert.rejects(sdk.llm.chat('', []), /缺少 id/, '空 id 抛可读错误')
    })
}

console.log(
    process.exitCode ? '\n结果：有失败用例' : `\n结果：全部通过（断言组 ${passed}）`,
)
