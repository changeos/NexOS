// shares.js —— 文件共享管理页面（SMB / NFS / WebDAV）
//
// 数据来源：os-api `/shares` 路由（ShareRouteHandler）。
// ShareInfo 结构（os-api::handlers::share::ShareInfo）：
//   { id, name, protocol, path, read_only, enabled }
// protocol 为小写字符串："smb" | "nfs" | "webdav" | ...
//
// 路由：
//   GET    /shares         列表
//   POST   /shares         创建（body=ShareInfo；服务端按 id 存）
//   DELETE /shares/:id     删除

(function () {
    'use strict';

    // —— API 封装（缺失时回退到原生 fetch） ——
    const API = typeof window !== 'undefined' && window.API ? window.API : {
        async get(url) { return parseOk(fetch(url)); },
        async post(url, body) { return parseOk(fetch(url, jsonReq('POST', body))); },
        async del(url) { return parseOk(fetch(url, { method: 'DELETE' })); },
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

    function jsonReq(method, body) {
        return {
            method,
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(body || {}),
        };
    }

    // —— 渲染辅助 ——
    function esc(s) {
        return String(s == null ? '' : s)
            .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
            .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
    }

    function protocolBadge(proto) {
        const p = String(proto || '').toLowerCase();
        const cls = { smb: 'badge-info', nfs: 'badge-ok', webdav: 'badge-warn' }[p] || 'badge-muted';
        return `<span class="badge ${cls}">${esc(p || '—')}</span>`;
    }

    function boolMark(v) {
        return v ? '✓' : '—';
    }

    function emptyRow(cols, msg) {
        return `<tr><td colspan="${cols}" class="muted center">${esc(msg)}</td></tr>`;
    }

    // —— 表格渲染 ——
    function renderSharesTable(shares) {
        const rows = Array.isArray(shares) && shares.length
            ? shares.map(renderShareRow).join('')
            : emptyRow(6, '暂无共享，点击“创建共享”新增。');

        return `
        <div class="page-head">
            <h2>文件共享</h2>
            <button class="btn btn-primary" id="share-create-btn">＋ 创建共享</button>
        </div>
        <div class="table-wrap">
            <table class="data-table">
                <thead>
                    <tr>
                        <th>共享名</th>
                        <th>协议</th>
                        <th>路径</th>
                        <th>只读</th>
                        <th>启用</th>
                        <th class="col-actions">操作</th>
                    </tr>
                </thead>
                <tbody>${rows}</tbody>
            </table>
        </div>
        <div id="share-modal-slot"></div>`;
    }

    function renderShareRow(s) {
        const id = esc(s.id);
        const name = esc(s.name || s.id);
        const path = esc(s.path || '');
        return `
        <tr>
            <td>${name}<div class="muted mono small">${id}</div></td>
            <td>${protocolBadge(s.protocol)}</td>
            <td class="mono">${path}</td>
            <td>${boolMark(s.read_only)}</td>
            <td>${s.enabled ? '<span class="badge badge-ok">启用</span>' : '<span class="badge badge-muted">禁用</span>'}</td>
            <td class="col-actions"><button class="btn btn-small btn-danger" data-action="delete" data-id="${id}">删除</button></td>
        </tr>`;
    }

    // —— 事件绑定 ——
    function bindShareEvents() {
        const slot = document.getElementById('content');

        slot.querySelectorAll('button[data-action="delete"]').forEach((btn) => {
            btn.addEventListener('click', async () => {
                const id = btn.dataset.id;
                if (!confirm(`确认删除共享 ${id}？`)) return;
                try {
                    await API.del(`/shares/${encodeURIComponent(id)}`);
                    await loadSharesPage();
                    showToast('共享已删除', 'success');
                } catch (e) {
                    showToast(`删除失败: ${e.message}`, 'error');
                }
            });
        });

        const createBtn = document.getElementById('share-create-btn');
        if (createBtn) createBtn.addEventListener('click', openCreateModal);
    }

    // —— 创建共享弹窗 ——
    function openCreateModal() {
        const slot = document.getElementById('share-modal-slot');
        if (!slot) return;
        slot.innerHTML = `
        <div class="modal-backdrop" id="share-modal">
            <div class="modal">
                <div class="modal-head"><h3>创建共享</h3><button class="modal-close" id="share-modal-close">×</button></div>
                <form id="share-create-form" class="form">
                    <label>共享名
                        <input type="text" name="name" placeholder="例如 media" required>
                    </label>
                    <label>协议
                        <select name="protocol">
                            <option value="smb">SMB</option>
                            <option value="nfs">NFS</option>
                            <option value="webdav">WebDAV</option>
                        </select>
                    </label>
                    <label>路径（数据集路径，如 /tank/media）
                        <input type="text" name="path" placeholder="/tank/media" required>
                    </label>
                    <label class="form-check"><input type="checkbox" name="read_only"> 只读</label>
                    <label class="form-check"><input type="checkbox" name="enabled" checked> 启用</label>
                    <div class="form-actions">
                        <button type="button" class="btn" id="share-modal-cancel">取消</button>
                        <button type="submit" class="btn btn-primary">创建</button>
                    </div>
                    <div class="form-msg muted small" id="share-form-msg"></div>
                </form>
            </div>
        </div>`;

        const close = () => { slot.innerHTML = ''; };
        document.getElementById('share-modal-close').addEventListener('click', close);
        document.getElementById('share-modal-cancel').addEventListener('click', close);
        document.getElementById('share-modal').addEventListener('click', (e) => {
            if (e.target.id === 'share-modal') close();
        });

        document.getElementById('share-create-form').addEventListener('submit', async (ev) => {
            ev.preventDefault();
            const fd = new FormData(ev.target);
            const body = {
                id: String(fd.get('name')),           // id 由前端用 name 兜底；真实后端可改
                name: String(fd.get('name')),
                protocol: String(fd.get('protocol')),
                path: String(fd.get('path')),
                read_only: fd.get('read_only') === 'on',
                enabled: fd.get('enabled') === 'on',
            };
            const msg = document.getElementById('share-form-msg');
            msg.textContent = '创建中…';
            try {
                await API.post('/shares', body);
                close();
                await loadSharesPage();
                showToast('共享已创建', 'success');
            } catch (e) {
                msg.textContent = `创建失败: ${e.message}`;
            }
        });
    }

    // —— 轻量提示 ——
    function showToast(msg, kind) {
        const host = document.getElementById('toast-slot') || document.body;
        const el = document.createElement('div');
        el.className = `toast toast-${kind || 'info'}`;
        el.textContent = msg;
        host.appendChild(el);
        setTimeout(() => el.remove(), 3500);
    }

    // —— 页面入口 ——
    async function loadSharesPage() {
        const content = document.getElementById('content');
        content.innerHTML = '<div class="loading">加载中...</div>';
        try {
            const data = await API.get('/shares');
            content.innerHTML = renderSharesTable(data);
            bindShareEvents();
        } catch (e) {
            content.innerHTML = `<div class="error">加载失败: ${esc(e.message)}</div>`;
        }
    }

    if (typeof module !== 'undefined' && module.exports) module.exports = { loadSharesPage };
    if (typeof window !== 'undefined') window.loadSharesPage = loadSharesPage;
})();
