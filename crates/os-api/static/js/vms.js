// vms.js —— 虚拟机管理页面（VM CRUD + 启停）
//
// 数据来源：os-api `/api/v1/vms*` 路由（ComputeRouteHandler）。
// VM 对象结构（os-compute::Vm，经 serde）：
//   {
//     id, name,
//     spec: { cpus:{vcpus,sockets,cores,threads}, memory_mb, disk_vol_id,
//             nics:[{bridge,mac?,model}], firmware:"bios"|"uefi" },
//     state: "running"|"stopped"|"paused"|"failed"|"migrating",  // snake_case
//     node_id?: string, created_at
//   }
//
// 路由：
//   GET    /api/v1/vms              列表
//   POST   /api/v1/vms              创建（body=VmSpec；id/name 服务端生成）
//   POST   /api/v1/vms/:id/start    启动
//   POST   /api/v1/vms/:id/stop     停止
//   DELETE /api/v1/vms/:id          销毁
//
// 红线：本文件不依赖 api.js（约定：window.API 提供 fetch 封装）。
// 若页面未注入 API，则回退到原生 fetch（带 JSON 解析 + 错误抛出）。

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

    function fmtMem(mb) {
        const n = Number(mb);
        if (!n) return '—';
        if (n >= 1024) return (n / 1024).toFixed(n % 1024 === 0 ? 0 : 1) + ' GiB';
        return n + ' MiB';
    }

    function stateBadge(state) {
        const s = String(state || '').toLowerCase();
        const cls = { running: 'badge-ok', paused: 'badge-warn', stopped: 'badge-muted', failed: 'badge-err', migrating: 'badge-info' }[s] || 'badge-muted';
        return `<span class="badge ${cls}">${esc(s || 'unknown')}</span>`;
    }

    function emptyRow(cols, msg) {
        return `<tr><td colspan="${cols}" class="muted center">${esc(msg)}</td></tr>`;
    }

    // —— 表格渲染 ——
    function renderVmsTable(vms) {
        const rows = Array.isArray(vms) && vms.length
            ? vms.map(renderVmRow).join('')
            : emptyRow(6, '暂无虚拟机，点击“创建 VM”新增。');

        return `
        <div class="page-head">
            <h2>虚拟机</h2>
            <button class="btn btn-primary" id="vm-create-btn">＋ 创建 VM</button>
        </div>
        <div class="table-wrap">
            <table class="data-table">
                <thead>
                    <tr>
                        <th>VM 名</th>
                        <th>状态</th>
                        <th>CPU</th>
                        <th>内存</th>
                        <th>节点</th>
                        <th class="col-actions">操作</th>
                    </tr>
                </thead>
                <tbody>${rows}</tbody>
            </table>
        </div>
        <div id="vm-modal-slot"></div>`;
    }

    function renderVmRow(vm) {
        const id = esc(vm.id);
        const name = esc(vm.name || vm.id);
        const vcpus = vm.spec && vm.spec.cpus ? vm.spec.cpus.vcpus : '—';
        const mem = vm.spec ? fmtMem(vm.spec.memory_mb) : '—';
        const node = vm.node_id ? esc(vm.node_id) : '<span class="muted">未调度</span>';
        const isRunning = String(vm.state).toLowerCase() === 'running';

        const startBtn = `<button class="btn btn-small btn-ok" data-action="start" data-id="${id}" ${isRunning ? 'disabled' : ''}>启动</button>`;
        const stopBtn = `<button class="btn btn-small btn-warn" data-action="stop" data-id="${id}" ${!isRunning ? 'disabled' : ''}>停止</button>`;
        const delBtn = `<button class="btn btn-small btn-danger" data-action="delete" data-id="${id}">删除</button>`;

        return `
        <tr>
            <td>${name}<div class="muted mono small">${id}</div></td>
            <td>${stateBadge(vm.state)}</td>
            <td>${vcpus} vCPU</td>
            <td>${mem}</td>
            <td>${node}</td>
            <td class="col-actions">${startBtn} ${stopBtn} ${delBtn}</td>
        </tr>`;
    }

    // —— 事件绑定 ——
    function bindVmEvents() {
        const slot = document.getElementById('content');

        // 表格内操作按钮（事件委托）
        slot.querySelectorAll('button[data-action]').forEach((btn) => {
            btn.addEventListener('click', async () => {
                const action = btn.dataset.action;
                const id = btn.dataset.id;
                if (action === 'delete') {
                    if (!confirm(`确认删除虚拟机 ${id}？此操作不可撤销。`)) return;
                    await doDeleteVm(id);
                } else if (action === 'start') {
                    await doLifecycleVm(id, 'start');
                } else if (action === 'stop') {
                    await doLifecycleVm(id, 'stop');
                }
            });
        });

        // 创建 VM
        const createBtn = document.getElementById('vm-create-btn');
        if (createBtn) createBtn.addEventListener('click', openCreateModal);
    }

    async function doLifecycleVm(id, action) {
        const slot = document.getElementById('content');
        try {
            await API.post(`/api/v1/vms/${encodeURIComponent(id)}/${action}`, {});
            await loadVmsPage();
        } catch (e) {
            showToast(`操作失败: ${e.message}`, 'error');
        }
    }

    async function doDeleteVm(id) {
        try {
            await API.del(`/api/v1/vms/${encodeURIComponent(id)}`);
            await loadVmsPage();
        } catch (e) {
            showToast(`删除失败: ${e.message}`, 'error');
        }
    }

    // —— 创建 VM 弹窗 ——
    function openCreateModal() {
        const slot = document.getElementById('vm-modal-slot');
        if (!slot) return;
        slot.innerHTML = `
        <div class="modal-backdrop" id="vm-modal">
            <div class="modal">
                <div class="modal-head"><h3>创建虚拟机</h3><button class="modal-close" id="vm-modal-close">×</button></div>
                <form id="vm-create-form" class="form">
                    <label>名称<span class="muted small">（可选；服务端按 id 生成）</span>
                        <input type="text" name="name" placeholder="例如 my-vm">
                    </label>
                    <label>CPU 数（vCPU）
                        <input type="number" name="cpus" min="1" max="128" value="2" required>
                    </label>
                    <label>内存（MiB）
                        <input type="number" name="memory" min="1" value="1024" required>
                    </label>
                    <label>磁盘卷 ID（zvol，如 tank/vm/disk1）
                        <input type="text" name="disk" value="tank/vm/disk1" required>
                    </label>
                    <div class="form-actions">
                        <button type="button" class="btn" id="vm-modal-cancel">取消</button>
                        <button type="submit" class="btn btn-primary">创建</button>
                    </div>
                    <div class="form-msg muted small" id="vm-form-msg"></div>
                </form>
            </div>
        </div>`;

        const close = () => { slot.innerHTML = ''; };
        document.getElementById('vm-modal-close').addEventListener('click', close);
        document.getElementById('vm-modal-cancel').addEventListener('click', close);
        document.getElementById('vm-modal').addEventListener('click', (e) => {
            if (e.target.id === 'vm-modal') close();
        });

        document.getElementById('vm-create-form').addEventListener('submit', async (ev) => {
            ev.preventDefault();
            const fd = new FormData(ev.target);
            const vcpus = Number(fd.get('cpus'));
            const body = {
                cpus: { vcpus, sockets: 1, cores: vcpus, threads: 1 },
                memory_mb: Number(fd.get('memory')),
                disk_vol_id: String(fd.get('disk')),
                nics: [{ bridge: 'br0', model: 'virtio' }],
                firmware: 'bios',
            };
            const msg = document.getElementById('vm-form-msg');
            msg.textContent = '创建中…';
            try {
                await API.post('/api/v1/vms', body);
                close();
                await loadVmsPage();
                showToast('虚拟机已创建', 'success');
            } catch (e) {
                msg.textContent = `创建失败: ${e.message}`;
            }
        });
    }

    // —— 轻量提示（无依赖；toast 容器由 index 负责渲染样式） ——
    function showToast(msg, kind) {
        const host = document.getElementById('toast-slot') || document.body;
        const el = document.createElement('div');
        el.className = `toast toast-${kind || 'info'}`;
        el.textContent = msg;
        host.appendChild(el);
        setTimeout(() => el.remove(), 3500);
    }

    // —— 页面入口 ——
    async function loadVmsPage() {
        const content = document.getElementById('content');
        content.innerHTML = '<div class="loading">加载中...</div>';
        try {
            const data = await API.get('/api/v1/vms');
            content.innerHTML = renderVmsTable(data);
            bindVmEvents();
        } catch (e) {
            content.innerHTML = `<div class="error">加载失败: ${esc(e.message)}</div>`;
        }
    }

    // 导出（兼容 CommonJS 与浏览器全局）
    if (typeof module !== 'undefined' && module.exports) module.exports = { loadVmsPage };
    if (typeof window !== 'undefined') window.loadVmsPage = loadVmsPage;
})();
