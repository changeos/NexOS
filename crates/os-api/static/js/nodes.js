// nodes.js —— 节点发现页面（LAN 内 OS 节点列表）
//
// 数据来源：os-api `/discover/nodes` 路由（DiscoverRouteHandler）。
// PeerNode 结构（os-discover::PeerNode，经 serde）：
//   {
//     node_id,                            // 节点 ID
//     endpoints: [string],                // 端点（含端口，如 ["10.0.0.5:8443"]）
//     version,                            // 软件版本
//     arch,                               // 架构（x86_64 / aarch64）
//     capabilities: {
//       supports_ha, storage_capacity_gb, network_gbps,
//       has_zfs, has_kvm, rdma, dpu
//     },
//     beacon_signature?: string           // 防伪签名（可空）
//   }
//
// 路由：
//   GET /discover/nodes      列出节点（与 GET /api/v1/nodes 等价）

(function () {
    'use strict';

    // —— API 封装（缺失时回退到原生 fetch） ——
    const API = typeof window !== 'undefined' && window.API ? window.API : {
        async get(url) { return parseOk(fetch(url)); },
    };

    async function parseOk(resp) {
        if (!resp.ok) {
            let msg = `HTTP ${resp.status}`;
            try { const j = await resp.json(); if (j && j.error) msg = j.error; } catch (_) { /* ignore */ }
            throw new Error(msg);
        }
        const text = await resp.text();
        return text ? JSON.parse(text) : null;
    }

    // —— 渲染辅助 ——
    function esc(s) {
        return String(s == null ? '' : s)
            .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
            .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
    }

    function fmtCap(gb) {
        const n = Number(gb);
        if (!n) return '—';
        if (n >= 1024) return (n / 1024).toFixed(n % 1024 === 0 ? 0 : 1) + ' TB';
        return n + ' GB';
    }

    // 能力徽章：KVM / ZFS / RDMA / DPU / HA
    function capabilityBadges(cap) {
        if (!cap) return '<span class="muted">—</span>';
        const badges = [];
        if (cap.has_kvm) badges.push('<span class="badge badge-info">KVM</span>');
        if (cap.has_zfs) badges.push('<span class="badge badge-ok">ZFS</span>');
        if (cap.rdma) badges.push('<span class="badge badge-warn">RDMA</span>');
        if (cap.dpu) badges.push('<span class="badge badge-info">DPU</span>');
        if (cap.supports_ha) badges.push('<span class="badge badge-err">HA</span>');
        return badges.length ? badges.join(' ') : '<span class="muted">无能力声明</span>';
    }

    // —— 卡片渲染 ——
    function renderNodesPage(nodes) {
        const cards = Array.isArray(nodes) && nodes.length
            ? nodes.map(renderNodeCard).join('')
            : `<div class="muted center">未发现任何节点。点击“刷新”重新扫描。</div>`;

        return `
        <div class="page-head">
            <h2>节点发现</h2>
            <button class="btn" id="node-refresh-btn">↻ 刷新</button>
        </div>
        <div class="cards-grid">${cards}</div>`;
    }

    function renderNodeCard(n) {
        const id = esc(n.node_id);
        const version = esc(n.version || '—');
        const arch = esc(n.arch || '—');
        const endpoints = Array.isArray(n.endpoints) && n.endpoints.length
            ? n.endpoints.map((e) => `<div class="mono">${esc(e)}</div>`).join('')
            : '<span class="muted">—</span>';
        const cap = n.capabilities || {};
        const storage = fmtCap(cap.storage_capacity_gb);
        const net = cap.network_gbps ? Number(cap.network_gbps) + ' Gbps' : '—';
        const sig = n.beacon_signature
            ? '<span class="badge badge-ok">已签名</span>'
            : '<span class="badge badge-muted">未签名</span>';

        return `
        <div class="card">
            <div class="card-head">
                <h3>${esc(n.node_id)}</h3>
                ${sig}
            </div>
            <dl class="card-meta">
                <dt>版本</dt><dd>${version}</dd>
                <dt>架构</dt><dd>${arch}</dd>
                <dt>能力</dt><dd>${capabilityBadges(cap)}</dd>
                <dt>存储容量</dt><dd>${storage}</dd>
                <dt>网络带宽</dt><dd>${net}</dd>
                <dt>端点</dt><dd>${endpoints}</dd>
            </dl>
            <div class="muted mono small card-id">${id}</div>
        </div>`;
    }

    // —— 事件绑定 ——
    function bindNodeEvents() {
        const refresh = document.getElementById('node-refresh-btn');
        if (refresh) refresh.addEventListener('click', () => loadNodesPage());
    }

    // —— 页面入口 ——
    async function loadNodesPage() {
        const content = document.getElementById('content');
        content.innerHTML = '<div class="loading">加载中...</div>';
        try {
            const data = await API.get('/discover/nodes');
            content.innerHTML = renderNodesPage(data);
            bindNodeEvents();
        } catch (e) {
            content.innerHTML = `<div class="error">加载失败: ${esc(e.message)}</div>`;
        }
    }

    if (typeof module !== 'undefined' && module.exports) module.exports = { loadNodesPage };
    if (typeof window !== 'undefined') window.loadNodesPage = loadNodesPage;
})();
