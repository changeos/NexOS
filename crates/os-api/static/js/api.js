/* ============================================================
   OS System Web UI — REST API 封装
   统一 fetch 调用、错误处理、具体端点封装。
   全部同源（baseUrl 为空），由 os-api 网关直接提供静态资源 + API。
   ============================================================ */

(function (global) {
    'use strict';

    /** API 封装对象（挂到全局 window.API） */
    const API = {
        /** 同源调用基址（网关在同一 origin 暴露静态资源与 API） */
        baseUrl: '',

        /** 统一请求超时（毫秒） */
        timeoutMs: 15000,

        /**
         * 调用 abort 超时的 fetch。
         * @param {string} path  请求路径（如 '/status'）
         * @param {RequestInit} [opts]  fetch 选项
         * @returns {Promise<any>} 解析后的 JSON
         */
        async request(path, opts) {
            const controller = new AbortController();
            const timer = setTimeout(() => controller.abort(), API.timeoutMs);
            try {
                const resp = await fetch(API.baseUrl + path, {
                    signal: controller.signal,
                    headers: { Accept: 'application/json' },
                    ...opts,
                });
                if (!resp.ok) {
                    // 尝试从响应体提取错误信息
                    let detail = '';
                    try {
                        const body = await resp.json();
                        detail = body.error || body.message || JSON.stringify(body);
                    } catch (_) {
                        try { detail = await resp.text(); } catch (__) { /* ignore */ }
                    }
                    const err = new Error(
                        `${resp.status} ${resp.statusText}${detail ? ' — ' + detail : ''}`
                    );
                    err.status = resp.status;
                    err.path = path;
                    throw err;
                }
                // 某些端点可能返回空体（204）；安全降级为 null
                const text = await resp.text();
                return text ? JSON.parse(text) : null;
            } finally {
                clearTimeout(timer);
            }
        },

        /** GET */
        get(path) { return API.request(path, { method: 'GET' }); },

        /** POST（body 为对象，自动 JSON 序列化） */
        post(path, body) {
            return API.request(path, {
                method: 'POST',
                headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
                body: body == null ? null : JSON.stringify(body),
            });
        },

        /** DELETE */
        delete(path) { return API.request(path, { method: 'DELETE' }); },

        // ============================================================
        // 具体端点封装（对齐 os-api 网关 25 路由）
        // ============================================================
        endpoints: {
            // —— 系统（handlers/system.rs）——
            status:    () => API.get('/status'),
            health:    () => API.get('/healthz'),
            version:   () => API.get('/api/v1/version'),
            virtCheck: () => API.get('/api/v1/system/virt-check'),

            // —— 存储（handlers/storage.rs）——
            pools:     (pool) => API.get(pool ? `/api/v1/pools/${encodeURIComponent(pool)}` : '/api/v1/pools'),
            datasets:  (pool) => API.get(pool ? `/api/v1/datasets?pool=${encodeURIComponent(pool)}` : '/api/v1/datasets'),
            snapshots: (ds)   => API.get(ds ? `/api/v1/snapshots?dataset=${encodeURIComponent(ds)}` : '/api/v1/snapshots'),

            // —— 计算（handlers/compute.rs）——
            vms:       () => API.get('/api/v1/vms'),
            vm:        (id) => API.get(`/api/v1/vms/${encodeURIComponent(id)}`),
            vmStart:   (id) => API.post(`/api/v1/vms/${encodeURIComponent(id)}/start`),
            vmStop:    (id) => API.post(`/api/v1/vms/${encodeURIComponent(id)}/stop`),
            vmDelete:  (id) => API.delete(`/api/v1/vms/${encodeURIComponent(id)}`),

            // —— 共享（handlers/share.rs）——
            shares:    () => API.get('/shares'),
            exports:   () => API.get('/api/v1/exports'),

            // —— 用户（handlers/user.rs）——
            users:     (includeDisabled) =>
                API.get('/api/v1/users' + (includeDisabled ? '?include_disabled=1' : '')),

            // —— 节点（handlers/discover.rs）——
            nodes:     () => API.get('/discover/nodes'),
            node:      (id) => API.get(`/api/v1/nodes/${encodeURIComponent(id)}`),
        },

        /** 兼容简写：API.status() / API.pools() ...（按任务约定） */
        status: function () { return API.endpoints.status(); },
        health: function () { return API.endpoints.health(); },
        version: function () { return API.endpoints.version(); },
        pools:  function () { return API.endpoints.pools(); },
        vms:    function () { return API.endpoints.vms(); },
        shares: function () { return API.endpoints.shares(); },
        users:  function () { return API.endpoints.users(); },
        nodes:  function () { return API.endpoints.nodes(); },
    };

    global.API = API;
})(window);
