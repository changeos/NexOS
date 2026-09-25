// users.js —— 用户管理页面
//
// 数据来源：os-api `/api/v1/users` 路由（UserRouteHandler）。
// UserInfo 结构（os-api::handlers::user::UserInfo）：
//   { id, name, roles: [string], enabled, is_guest }
// roles 例：["admin"] / ["operator"] / ["guest"]
//
// 路由：
//   GET    /api/v1/users          列表
//   POST   /api/v1/users          创建（body=UserInfo）
//   DELETE /api/v1/users/:id      删除

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

    function roleBadges(roles) {
        const arr = Array.isArray(roles) ? roles : [];
        if (!arr.length) return '<span class="muted">—</span>';
        const cls = { admin: 'badge-err', operator: 'badge-info', guest: 'badge-muted' };
        return arr.map((r) => `<span class="badge ${cls[String(r).toLowerCase()] || 'badge-muted'}">${esc(r)}</span>`).join(' ');
    }

    function emptyRow(cols, msg) {
        return `<tr><td colspan="${cols}" class="muted center">${esc(msg)}</td></tr>`;
    }

    // —— 表格渲染 ——
    function renderUsersTable(users) {
        const rows = Array.isArray(users) && users.length
            ? users.map(renderUserRow).join('')
            : emptyRow(5, '暂无用户，点击“创建用户”新增。');

        return `
        <div class="page-head">
            <h2>用户</h2>
            <button class="btn btn-primary" id="user-create-btn">＋ 创建用户</button>
        </div>
        <div class="table-wrap">
            <table class="data-table">
                <thead>
                    <tr>
                        <th>用户名</th>
                        <th>角色</th>
                        <th>启用</th>
                        <th>访客</th>
                        <th class="col-actions">操作</th>
                    </tr>
                </thead>
                <tbody>${rows}</tbody>
            </table>
        </div>
        <div id="user-modal-slot"></div>`;
    }

    function renderUserRow(u) {
        const id = esc(u.id);
        const name = esc(u.name || u.id);
        const guest = u.is_guest
            ? '<span class="badge badge-warn">访客</span>'
            : '<span class="muted">—</span>';
        const enabled = u.enabled
            ? '<span class="badge badge-ok">启用</span>'
            : '<span class="badge badge-muted">禁用</span>';
        return `
        <tr>
            <td>${name}<div class="muted mono small">${id}</div></td>
            <td>${roleBadges(u.roles)}</td>
            <td>${enabled}</td>
            <td>${guest}</td>
            <td class="col-actions"><button class="btn btn-small btn-danger" data-action="delete" data-id="${id}">删除</button></td>
        </tr>`;
    }

    // —— 事件绑定 ——
    function bindUserEvents() {
        const slot = document.getElementById('content');

        slot.querySelectorAll('button[data-action="delete"]').forEach((btn) => {
            btn.addEventListener('click', async () => {
                const id = btn.dataset.id;
                if (!confirm(`确认删除用户 ${id}？`)) return;
                try {
                    await API.del(`/api/v1/users/${encodeURIComponent(id)}`);
                    await loadUsersPage();
                    showToast('用户已删除', 'success');
                } catch (e) {
                    showToast(`删除失败: ${e.message}`, 'error');
                }
            });
        });

        const createBtn = document.getElementById('user-create-btn');
        if (createBtn) createBtn.addEventListener('click', openCreateModal);
    }

    // —— 创建用户弹窗 ——
    function openCreateModal() {
        const slot = document.getElementById('user-modal-slot');
        if (!slot) return;
        slot.innerHTML = `
        <div class="modal-backdrop" id="user-modal">
            <div class="modal">
                <div class="modal-head"><h3>创建用户</h3><button class="modal-close" id="user-modal-close">×</button></div>
                <form id="user-create-form" class="form">
                    <label>用户名
                        <input type="text" name="name" placeholder="例如 alice" required>
                    </label>
                    <label>角色（逗号分隔，如 admin, operator）
                        <input type="text" name="roles" placeholder="operator" value="operator">
                    </label>
                    <label class="form-check"><input type="checkbox" name="is_guest"> 访客身份</label>
                    <label class="form-check"><input type="checkbox" name="enabled" checked> 启用</label>
                    <div class="form-actions">
                        <button type="button" class="btn" id="user-modal-cancel">取消</button>
                        <button type="submit" class="btn btn-primary">创建</button>
                    </div>
                    <div class="form-msg muted small" id="user-form-msg"></div>
                </form>
            </div>
        </div>`;

        const close = () => { slot.innerHTML = ''; };
        document.getElementById('user-modal-close').addEventListener('click', close);
        document.getElementById('user-modal-cancel').addEventListener('click', close);
        document.getElementById('user-modal').addEventListener('click', (e) => {
            if (e.target.id === 'user-modal') close();
        });

        document.getElementById('user-create-form').addEventListener('submit', async (ev) => {
            ev.preventDefault();
            const fd = new FormData(ev.target);
            const name = String(fd.get('name')).trim();
            const rolesRaw = String(fd.get('roles') || '').trim();
            const roles = rolesRaw
                ? rolesRaw.split(',').map((r) => r.trim()).filter(Boolean)
                : [];
            const body = {
                id: name,
                name,
                roles,
                enabled: fd.get('enabled') === 'on',
                is_guest: fd.get('is_guest') === 'on',
            };
            const msg = document.getElementById('user-form-msg');
            msg.textContent = '创建中…';
            try {
                await API.post('/api/v1/users', body);
                close();
                await loadUsersPage();
                showToast('用户已创建', 'success');
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
    async function loadUsersPage() {
        const content = document.getElementById('content');
        content.innerHTML = '<div class="loading">加载中...</div>';
        try {
            const data = await API.get('/api/v1/users');
            content.innerHTML = renderUsersTable(data);
            bindUserEvents();
        } catch (e) {
            content.innerHTML = `<div class="error">加载失败: ${esc(e.message)}</div>`;
        }
    }

    if (typeof module !== 'undefined' && module.exports) module.exports = { loadUsersPage };
    if (typeof window !== 'undefined') window.loadUsersPage = loadUsersPage;
})();
