/* ============================================================
   OS System Web UI — 仪表盘页面逻辑 + 简易 hash 路由
   - 启动时渲染仪表盘（系统概览 + 存储池列表 + VM/共享计数）
   - 每 10 秒自动刷新
   - hash route 切换页面：#dashboard / #storage / #vms / ...
   - 非仪表盘页面渲染占位（其余页面由其他子代理实现）
   ============================================================ */

(function () {
    'use strict';

    const REFRESH_MS = 10_000;

    /** DOM 引用 */
    const el = {
        content:   document.getElementById('content'),
        version:   document.getElementById('version-badge'),
        navVersion:document.getElementById('nav-version'),
        healthDot: document.getElementById('health-dot'),
        healthText:document.getElementById('health-text'),
        osName:   document.getElementById('topbar-osname'),
        navItems:  Array.from(document.querySelectorAll('.nav-item')),
        menuToggle:document.getElementById('menu-toggle'),
        sidebar:   document.getElementById('sidebar'),
    };

    /** 当前活动的刷新定时器 */
    let refreshTimer = null;

    // ============================================================
    // 工具函数
    // ============================================================

    /** 字节 → 人类可读（二进制，1024） */
    function fmtBytes(n) {
        if (n == null || isNaN(n)) return '—';
        const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
        let v = Number(n), i = 0;
        while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
        return `${v.toFixed(v < 10 && i > 0 ? 1 : 0)} ${units[i]}`;
    }

    /** 秒 → 可读时长（如 1d 3h 20m） */
    function fmtUptime(s) {
        if (s == null || isNaN(s) || s < 0) return '—';
        const d = Math.floor(s / 86400);
        const h = Math.floor((s % 86400) / 3600);
        const m = Math.floor((s % 3600) / 60);
        if (d > 0) return `${d}d ${h}h ${m}m`;
        if (h > 0) return `${h}h ${m}m`;
        return `${m}m`;
    }

    /** HTML 转义（防 XSS） */
    function esc(s) {
        return String(s == null ? '' : s)
            .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
            .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
    }

    /** 容量进度条 HTML */
    function capacityBar(cap) {
        if (!cap || !cap.total_bytes) {
            return '<span class="muted">未配置</span>';
        }
        const used = Number(cap.used_bytes || 0);
        const total = Number(cap.total_bytes);
        const pct = Math.min(100, Math.round((used / total) * 100));
        const cls = pct >= 90 ? 'err' : (pct >= 75 ? 'warn' : '');
        return `
            <div class="capacity">
                <div class="progress"><div class="progress-fill ${cls}" style="width:${pct}%"></div></div>
                <span class="capacity-text">${pct}% · ${fmtBytes(used)} / ${fmtBytes(total)}</span>
            </div>`;
    }

    /** 健康徽章（health 字段为 snake_case：healthy/degraded/unhealthy/unknown） */
    function healthBadge(h) {
        const v = (h || 'unknown').toLowerCase();
        return `<span class="badge ${esc(v)}">${esc(v)}</span>`;
    }

    /** 设置顶部健康指示灯 */
    function setHealthIndicator(h) {
        const v = (h || '').toLowerCase();
        el.healthDot.className = 'health-dot';
        let cls = 'unknown', text = '未知';
        if (v === 'healthy')   { cls = 'ok';  text = '健康'; }
        else if (v === 'degraded')  { cls = 'warn'; text = '降级'; }
        else if (v === 'unhealthy') { cls = 'err';  text = '故障'; }
        el.healthDot.classList.add(cls);
        el.healthText.textContent = text;
    }

    // ============================================================
    // 仪表盘渲染
    // ============================================================

    /** 渲染单次错误提示 */
    function errorBox(msg) {
        return `<div class="error-box">⚠ 数据加载失败：${esc(msg)}</div>`;
    }

    /**
     * 加载并渲染仪表盘。并发拉取 status / pools / vms / shares，
     * 单个端点失败不影响其余（用 Promise.allSettled）。
     */
    async function renderDashboard() {
        const ts = new Date().toLocaleTimeString();
        el.content.innerHTML = `
            <div class="loading">正在加载系统状态…</div>`;

        const results = await Promise.allSettled([
            API.status(),
            API.pools(),
            API.vms(),
            API.shares(),
        ]);
        const [rStatus, rPools, rVms, rShares] = results;

        let html = '';

        // —— 顶部版本 / 健康灯（即便 status 失败也尝试用 version 兜底）——
        if (rStatus.status === 'fulfilled') {
            const s = rStatus.value || {};
            if (s.version) {
                el.version.textContent = 'v' + s.version;
                el.navVersion.textContent = 'v' + s.version;
            }
            if (s.hostname) el.osName.textContent = 'OS · ' + s.hostname;
            setHealthIndicator(s.health);
        } else {
            // 单独再尝试 version 端点（status 聚合有时会因子探测失败而整体报错）
            try {
                const v = await API.version();
                if (v && v.version) {
                    el.version.textContent = 'v' + v.version;
                    el.navVersion.textContent = 'v' + v.version;
                }
            } catch (_) { /* 忽略 */ }
            setHealthIndicator('unknown');
        }

        // —— 错误提示聚合 ——
        const errs = results.filter(r => r.status === 'rejected');
        if (errs.length) {
            html += errorBox(errs.map(e => e.reason && e.reason.message || String(e.reason)).join('; '));
        }

        // —— 系统概览卡片 ——
        const status = rStatus.status === 'fulfilled' ? (rStatus.value || {}) : {};
        const cpuVirt = status.cpu_virt;
        const virtText = cpuVirt && (cpuVirt.usable || cpuVirt.is_usable)
            ? '可用' : (cpuVirt && cpuVirt.error ? '检测失败' : '不支持');
        const pools = rPools.status === 'fulfilled' ? (rPools.value || []) : [];
        const vms   = rVms.status === 'fulfilled'   ? (rVms.value || []) : [];
        const shares= rShares.status === 'fulfilled'? (rShares.value || []) : [];

        html += `
            <div class="page-title">系统概览</div>
            <div class="refresh-bar" style="margin-bottom:14px">最近更新：${ts} · 每 ${REFRESH_MS / 1000}s 自动刷新</div>

            <div class="grid-overview">
                <div class="card">
                    <div class="card-title">主机名</div>
                    <div class="card-value">${esc(status.hostname || '—')}</div>
                </div>
                <div class="card">
                    <div class="card-title">版本</div>
                    <div class="card-value">v${esc(status.version || '—')}</div>
                </div>
                <div class="card">
                    <div class="card-title">健康状态</div>
                    <div class="card-value">${healthBadge(status.health)}</div>
                </div>
                <div class="card">
                    <div class="card-title">CPU 虚拟化</div>
                    <div class="card-value">${esc(virtText)}</div>
                </div>
                <div class="card">
                    <div class="card-title">存储池</div>
                    <div class="card-value">${pools.length}</div>
                </div>
                <div class="card">
                    <div class="card-title">虚拟机</div>
                    <div class="card-value">${vms.length}</div>
                </div>
                <div class="card">
                    <div class="card-title">共享</div>
                    <div class="card-value">${shares.length}</div>
                </div>
                <div class="card">
                    <div class="card-title">运行时间</div>
                    <div class="card-value">${esc(fmtUptime(status.uptime))}</div>
                </div>
            </div>`;

        // —— 存储池列表 ——
        html += `
            <div class="section">
                <div class="section-head">
                    <h2>存储池</h2>
                    <span class="hint">${pools.length} 个池</span>
                </div>
                <div class="card" style="padding:0;overflow:hidden">
                    ${renderPoolsTable(pools)}
                </div>
            </div>`;

        // —— 虚拟机快览 ——
        html += `
            <div class="section">
                <div class="section-head">
                    <h2>虚拟机</h2>
                    <span class="hint">${vms.length} 台</span>
                </div>
                <div class="card" style="padding:0;overflow:hidden">
                    ${renderVmsTable(vms)}
                </div>
            </div>`;

        // —— 共享快览 ——
        html += `
            <div class="section">
                <div class="section-head">
                    <h2>共享</h2>
                    <span class="hint">${shares.length} 个</span>
                </div>
                <div class="card" style="padding:0;overflow:hidden">
                    ${renderSharesTable(shares)}
                </div>
            </div>`;

        el.content.innerHTML = html;
    }

    function renderPoolsTable(pools) {
        if (!pools || !pools.length) {
            return '<div class="empty-hint">暂无存储池</div>';
        }
        const rows = pools.map(p => `
            <tr>
                <td><strong>${esc(p.name || p.id)}</strong></td>
                <td>${healthBadge(p.health)}</td>
                <td>${capacityBar(p.capacity)}</td>
                <td class="num">${esc((p.vdevs || []).length)}</td>
            </tr>`).join('');
        return `
            <table class="table">
                <thead><tr>
                    <th>名称</th><th>健康</th><th>容量</th><th class="num">vdev 数</th>
                </tr></thead>
                <tbody>${rows}</tbody>
            </table>`;
    }

    function renderVmsTable(vms) {
        if (!vms || !vms.length) {
            return '<div class="empty-hint">暂无虚拟机</div>';
        }
        const rows = vms.map(v => {
            const state = (v.state || 'stopped').toLowerCase();
            return `
                <tr>
                    <td><strong>${esc(v.name || v.id)}</strong></td>
                    <td><span class="badge ${esc(state)}">${esc(state)}</span></td>
                    <td class="muted">${esc(v.node_id || '未调度')}</td>
                    <td class="num muted">${esc((v.spec && v.spec.vcpus) || '—')} vCPU</td>
                </tr>`;
        }).join('');
        return `
            <table class="table">
                <thead><tr>
                    <th>名称</th><th>状态</th><th>节点</th><th class="num">规格</th>
                </tr></thead>
                <tbody>${rows}</tbody>
            </table>`;
    }

    function renderSharesTable(shares) {
        if (!shares || !shares.length) {
            return '<div class="empty-hint">暂无共享</div>';
        }
        const rows = shares.map(s => {
            const proto = (s.protocol || '').toLowerCase();
            const enabledCls = s.enabled ? 'healthy' : 'unknown';
            const enabledText = s.enabled ? '启用' : '禁用';
            return `
                <tr>
                    <td><strong>${esc(s.name || s.id)}</strong></td>
                    <td>${esc(proto)}</td>
                    <td class="muted">${esc(s.path || '—')}</td>
                    <td>${esc(s.read_only ? '只读' : '读写')}</td>
                    <td><span class="badge ${enabledCls}">${enabledText}</span></td>
                </tr>`;
        }).join('');
        return `
            <table class="table">
                <thead><tr>
                    <th>名称</th><th>协议</th><th>路径</th><th>权限</th><th>状态</th>
                </tr></thead>
                <tbody>${rows}</tbody>
            </table>`;
    }

    // ============================================================
    // 简易 hash 路由
    // ============================================================

    const routes = {
        dashboard: { render: renderDashboard, hasRefresh: true,  title: '仪表盘' },
        storage:   { placeholder: '存储管理页面（开发中）', title: '存储' },
        vms:       { placeholder: '虚拟机管理页面（开发中）', title: '虚拟机' },
        shares:    { placeholder: '共享管理页面（开发中）', title: '共享' },
        users:     { placeholder: '用户管理页面（开发中）', title: '用户' },
        nodes:     { placeholder: '节点管理页面（开发中）', title: '节点' },
        settings:  { placeholder: '系统设置页面（开发中）', title: '设置' },
    };

    function stopRefresh() {
        if (refreshTimer) { clearInterval(refreshTimer); refreshTimer = null; }
    }

    function currentRoute() {
        const h = (location.hash || '#dashboard').replace(/^#/, '');
        return routes[h] ? h : 'dashboard';
    }

    function setActiveNav(route) {
        el.navItems.forEach(a => {
            a.classList.toggle('active', a.dataset.route === route);
        });
    }

    async function route() {
        const name = currentRoute();
        const def = routes[name];
        setActiveNav(name);
        stopRefresh();

        if (def.render) {
            await def.render();
            if (def.hasRefresh) {
                refreshTimer = setInterval(def.render, REFRESH_MS);
            }
        } else {
            el.content.innerHTML = `
                <div class="page-title">${esc(def.title)}</div>
                <div class="card">
                    <div class="empty-hint">${esc(def.placeholder)}</div>
                </div>`;
        }

        // 手机端点击后收起侧栏
        if (window.matchMedia('(max-width: 900px)').matches) {
            el.sidebar.classList.remove('open');
        }
    }

    // ============================================================
    // 启动
    // ============================================================

    window.addEventListener('hashchange', route);
    el.menuToggle.addEventListener('click', () => el.sidebar.classList.toggle('open'));

    // 首次渲染
    route();
})();
