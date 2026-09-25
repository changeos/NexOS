/**
 * storage.js —— OS Web UI 存储管理页面逻辑（纯 Vanilla JS，无框架）。
 *
 * 由 index.html 的导航调用 loadStoragePage()，动态渲染到 #content div。
 * 通过 fetch() 调用 REST API：
 *   - GET    /api/v1/pools            列出存储池
 *   - POST   /api/v1/pools            创建存储池
 *   - GET    /api/v1/datasets[?pool=] 列出数据集（可按池筛选）
 *   - GET    /api/v1/snapshots[?dataset=] 列出快照（可按数据集筛选）
 *
 * 后端响应字段（serde 序列化结果）：
 *   Pool     { id, name, vdevs, capacity:{used_bytes,total_bytes}, health }
 *            health ∈ {"healthy","degraded","unhealthy","unknown"} (snake_case)
 *   Dataset  { id, pool, name, used_bytes, avail_bytes, mounted, encryption }
 *            encryption ∈ {"off","unlocked","locked"} (snake_case)
 *   Snapshot { id, dataset, created(RFC3339), used_bytes }
 *
 * 本文件只负责存储管理页；不写 index.html / css / api.js（避免与其他子代理冲突）。
 */
(function (global) {
  "use strict";

  // ===========================================================================
  // 工具函数
  // ===========================================================================

  /**
   * 字节容量自动格式化：根据数值大小选 KB/MB/GB/TB/PB 单位。
   * @param {number} bytes 字节数
   * @returns {string} 人类可读的容量字符串（如 "1.50 TB"）；非数字返回 "-"
   */
  function formatBytes(bytes) {
    if (bytes === null || bytes === undefined || Number.isNaN(Number(bytes))) {
      return "-";
    }
    const n = Number(bytes);
    if (n < 0) return "-";
    const UNITS = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
    if (n < 1) return "0 B";
    const i = Math.min(
      UNITS.length - 1,
      Math.floor(Math.log(n) / Math.log(1024))
    );
    const val = n / Math.pow(1024, i);
    // 整数（B）不带小数；其余保留 2 位小数
    const formatted = i === 0 ? val.toFixed(0) : val.toFixed(2);
    return formatted + " " + UNITS[i];
  }

  /**
   * HTML 转义：防止用户/后端字符串注入到 innerHTML（XSS 防护）。
   * @param {string} s
   * @returns {string}
   */
  function escapeHtml(s) {
    if (s === null || s === undefined) return "";
    return String(s)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  /**
   * 统一的 fetch 封装：返回 JSON，非 2xx 时抛错。
   * @param {string} url
   * @param {object} [opts] fetch 第二参（method/body/headers...）
   * @returns {Promise<any>}
   */
  async function fetchJson(url, opts) {
    const resp = await fetch(url, opts);
    let data = null;
    try {
      data = await resp.json();
    } catch (_) {
      /* 非 JSON 响应体 */
    }
    if (!resp.ok) {
      const msg =
        (data && (data.error || data.message)) ||
        "HTTP " + resp.status + " " + resp.statusText;
      throw new Error(msg);
    }
    return data;
  }

  /**
   * 生成加载中 spinner DOM 字符串（含提示文案）。
   * @param {string} [text="加载中..."]
   * @returns {string} HTML
   */
  function spinnerHtml(text) {
    const t = text || "加载中...";
    return (
      '<div class="storage-loading" style="text-align:center;padding:32px;color:#666;">' +
      '<span class="storage-spinner" aria-hidden="true"></span> ' +
      escapeHtml(t) +
      "</div>"
    );
  }

  /**
   * 构造一个简单的内联错误提示区块。
   * @param {string} msg 错误消息
   * @returns {string} HTML
   */
  function errorBoxHtml(msg) {
    return (
      '<div class="storage-error" style="color:#b00020;background:#fde8ea;border:1px solid #f5c0c6;' +
      'padding:12px;border-radius:4px;margin:8px 0;">' +
      "加载失败：" +
      escapeHtml(msg) +
      "</div>"
    );
  }

  // 简单的 spinner CSS 旋转动画（若 index.html 未定义则注入一次，避免冲突）
  (function injectSpinnerCss() {
    if (document.getElementById("storage-spinner-css")) return;
    const style = document.createElement("style");
    style.id = "storage-spinner-css";
    style.textContent =
      ".storage-spinner{display:inline-block;width:14px;height:14px;" +
      "border:2px solid #ccc;border-top-color:#2563eb;border-radius:50%;" +
      "vertical-align:middle;animation:storage-spin .8s linear infinite;}" +
      "@keyframes storage-spin{to{transform:rotate(360deg);}}";
    document.head.appendChild(style);
  })();

  // ===========================================================================
  // 健康状态徽章（彩色）
  // ===========================================================================

  /**
   * 健康状态 → {label, color, bg} 用于渲染彩色徽章。
   * 后端 health 枚举为 snake_case：healthy/degraded/unhealthy/unknown。
   * @param {string} health
   */
  function healthBadge(health) {
    const map = {
      healthy: { label: "健康", color: "#15803d", bg: "#dcfce7" },
      degraded: { label: "降级", color: "#b45309", bg: "#fef3c7" },
      unhealthy: { label: "故障", color: "#b91c1c", bg: "#fee2e2" },
      unknown: { label: "未知", color: "#475569", bg: "#e2e8f0" },
    };
    const m = map[String(health || "").toLowerCase()] || map.unknown;
    return (
      '<span class="health-badge" style="display:inline-block;padding:2px 10px;' +
      "border-radius:10px;font-size:12px;font-weight:600;color:" +
      m.color +
      ";background:" +
      m.bg +
      ';">' +
      m.label +
      "</span>"
    );
  }

  /**
   * 加密状态徽章。
   * 后端 encryption 枚举为 snake_case：off/unlocked/locked。
   * @param {string} enc
   */
  function encryptionBadge(enc) {
    const map = {
      off: { label: "未加密", color: "#475569", bg: "#e2e8f0" },
      unlocked: { label: "已解锁", color: "#15803d", bg: "#dcfce7" },
      locked: { label: "已锁定", color: "#b91c1c", bg: "#fee2e2" },
    };
    const m = map[String(enc || "").toLowerCase()] || map.off;
    return (
      '<span style="display:inline-block;padding:2px 10px;border-radius:10px;' +
      "font-size:12px;color:" +
      m.color +
      ";background:" +
      m.bg +
      ';">' +
      m.label +
      "</span>"
    );
  }

  /**
   * 使用率进度条（百分比）。
   * @param {number} ratio 0~1
   * @returns {string} HTML
   */
  function usageBar(ratio) {
    const r = Math.max(0, Math.min(1, Number(ratio) || 0));
    const pct = (r * 100).toFixed(1);
    // 颜色：>90% 红，>75% 橙，其余绿
    let color = "#16a34a";
    if (r >= 0.9) color = "#b91c1c";
    else if (r >= 0.75) color = "#d97706";
    return (
      '<div style="display:flex;align-items:center;gap:8px;">' +
      '<div style="flex:1;height:8px;background:#e5e7eb;border-radius:4px;overflow:hidden;">' +
      '<div style="width:' +
      pct +
      "%;height:100%;background:" +
      color +
      ';"></div></div>' +
      '<span style="font-size:12px;color:#374151;min-width:42px;text-align:right;">' +
      pct +
      "%</span></div>"
    );
  }

  // ===========================================================================
  // 页面主入口：loadStoragePage()
  // ===========================================================================

  /**
   * 加载存储管理页面：渲染顶部 tab（池/数据集/快照）到 #content。
   * 由 index.html 导航调用。维护一个简单的模块级当前 tab 状态。
   * @param {HTMLElement} [contentEl] 渲染目标；默认 document.getElementById("content")
   */
  function loadStoragePage(contentEl) {
    const root = contentEl || document.getElementById("content");
    if (!root) {
      console.error("loadStoragePage: 找不到 #content 容器");
      return;
    }

    root.innerHTML =
      '<div class="storage-page">' +
      '<h2 style="margin:0 0 16px 0;">存储管理</h2>' +
      '<div class="storage-tabs" style="display:flex;gap:4px;border-bottom:1px solid #e5e7eb;margin-bottom:16px;">' +
      tabBtnHtml("pools", "存储池", true) +
      tabBtnHtml("datasets", "数据集", false) +
      tabBtnHtml("snapshots", "快照", false) +
      "</div>" +
      '<div id="storage-tab-content"></div>' +
      "</div>";

    // 绑定 tab 切换
    const tabs = root.querySelectorAll(".storage-tab-btn");
    tabs.forEach(function (btn) {
      btn.addEventListener("click", function () {
        tabs.forEach(function (b) {
          b.setAttribute("data-active", "false");
          b.style.cssText = tabBtnCss(false);
        });
        btn.setAttribute("data-active", "true");
        btn.style.cssText = tabBtnCss(true);
        const name = btn.getAttribute("data-tab");
        switchTab(name);
      });
    });

    // 默认加载池
    switchTab("pools");
  }

  /**
   * 渲染一个 tab 按钮。
   * @param {string} name tab 标识
   * @param {string} label 显示文案
   * @param {boolean} active 是否激活
   */
  function tabBtnHtml(name, label, active) {
    return (
      '<button type="button" class="storage-tab-btn" data-tab="' +
      name +
      '" data-active="' +
      active +
      '" style="' +
      tabBtnCss(active) +
      '">' +
      escapeHtml(label) +
      "</button>"
    );
  }

  function tabBtnCss(active) {
    const base =
      "padding:8px 16px;border:1px solid #e5e7eb;border-bottom:none;" +
      "border-radius:6px 6px 0 0;cursor:pointer;background:#fff;font-size:14px;";
    return active
      ? base + "background:#2563eb;color:#fff;border-color:#2563eb;"
      : base + "color:#374151;";
  }

  /**
   * 切换到指定 tab 并加载其内容。
   * @param {string} name "pools" | "datasets" | "snapshots"
   */
  function switchTab(name) {
    const tc = document.getElementById("storage-tab-content");
    if (!tc) return;
    if (name === "pools") {
      renderPoolsPanel(tc);
    } else if (name === "datasets") {
      renderDatasetsPanel(tc);
    } else if (name === "snapshots") {
      renderSnapshotsPanel(tc);
    }
  }

  // ===========================================================================
  // 存储池面板
  // ===========================================================================

  function renderPoolsPanel(tc) {
    tc.innerHTML =
      '<div class="storage-pools-panel">' +
      '<div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:12px;">' +
      "<h3 style=\"margin:0;\">存储池</h3>" +
      '<button type="button" id="storage-create-pool-btn" ' +
      'style="padding:6px 14px;background:#2563eb;color:#fff;border:none;border-radius:4px;cursor:pointer;">' +
      "+ 创建池</button>" +
      "</div>" +
      '<div id="storage-pools-list">' +
      spinnerHtml("加载存储池...") +
      "</div>" +
      '<div id="storage-pool-form" style="display:none;"></div>' +
      "</div>";

    // 绑定创建池按钮
    const createBtn = document.getElementById("storage-create-pool-btn");
    if (createBtn) {
      createBtn.addEventListener("click", function () {
        toggleCreatePoolForm();
      });
    }

    loadPoolsList();
  }

  async function loadPoolsList() {
    const box = document.getElementById("storage-pools-list");
    if (!box) return;
    box.innerHTML = spinnerHtml("加载存储池...");
    try {
      const pools = await fetchJson("/api/v1/pools");
      renderPoolsTable(box, Array.isArray(pools) ? pools : []);
    } catch (e) {
      box.innerHTML = errorBoxHtml(e.message || String(e));
    }
  }

  function renderPoolsTable(box, pools) {
    if (!pools.length) {
      box.innerHTML =
        '<div style="padding:24px;text-align:center;color:#666;">' +
        "暂无存储池，点击右上角「创建池」添加。</div>";
      return;
    }
    let html =
      '<table style="width:100%;border-collapse:collapse;font-size:14px;">' +
      "<thead><tr style=\"background:#f9fafb;text-align:left;\">" +
      '<th style="padding:10px;border-bottom:1px solid #e5e7eb;">池名</th>' +
      '<th style="padding:10px;border-bottom:1px solid #e5e7eb;">健康</th>' +
      '<th style="padding:10px;border-bottom:1px solid #e5e7eb;">总容量</th>' +
      '<th style="padding:10px;border-bottom:1px solid #e5e7eb;">已用</th>' +
      '<th style="padding:10px;border-bottom:1px solid #e5e7eb;">使用率</th>' +
      "</tr></thead><tbody>";
    for (let i = 0; i < pools.length; i++) {
      const p = pools[i];
      const cap = p.capacity || {};
      const total = Number(cap.total_bytes) || 0;
      const used = Number(cap.used_bytes) || 0;
      const ratio = total > 0 ? used / total : 0;
      html +=
        "<tr>" +
        '<td style="padding:10px;border-bottom:1px solid #f3f4f6;font-weight:500;">' +
        escapeHtml(p.name || p.id) +
        "</td>" +
        '<td style="padding:10px;border-bottom:1px solid #f3f4f6;">' +
        healthBadge(p.health) +
        "</td>" +
        '<td style="padding:10px;border-bottom:1px solid #f3f4f6;">' +
        formatBytes(total) +
        "</td>" +
        '<td style="padding:10px;border-bottom:1px solid #f3f4f6;">' +
        formatBytes(used) +
        "</td>" +
        '<td style="padding:10px;border-bottom:1px solid #f3f4f6;min-width:200px;">' +
        usageBar(ratio) +
        "</td>" +
        "</tr>";
    }
    html += "</tbody></table>";
    box.innerHTML = html;
  }

  /**
   * 切换显示/隐藏创建池表单。
   */
  function toggleCreatePoolForm() {
    const form = document.getElementById("storage-pool-form");
    if (!form) return;
    if (form.style.display === "none") {
      form.style.display = "block";
      form.innerHTML = buildCreatePoolFormHtml();
      const cancelBtn = document.getElementById("pool-cancel-btn");
      const submitBtn = document.getElementById("pool-submit-btn");
      if (cancelBtn) {
        cancelBtn.addEventListener("click", function () {
          form.style.display = "none";
          form.innerHTML = "";
        });
      }
      if (submitBtn) {
        submitBtn.addEventListener("click", submitCreatePool);
      }
    } else {
      form.style.display = "none";
      form.innerHTML = "";
    }
  }

  function buildCreatePoolFormHtml() {
    return (
      '<div style="margin-top:12px;padding:16px;border:1px solid #e5e7eb;border-radius:6px;background:#fafafa;">' +
      '<h4 style="margin:0 0 12px 0;">创建存储池</h4>' +
      '<div style="margin-bottom:12px;">' +
      '<label style="display:block;margin-bottom:4px;font-size:13px;">池名</label>' +
      '<input type="text" id="pool-name-input" placeholder="例如 tank" ' +
      'style="width:100%;max-width:320px;padding:6px 8px;border:1px solid #d1d5db;border-radius:4px;" />' +
      "</div>" +
      '<div style="margin-bottom:12px;">' +
      '<label style="display:block;margin-bottom:4px;font-size:13px;">vdevs 磁盘路径</label>' +
      '<div style="font-size:12px;color:#666;margin-bottom:6px;">每行一个 vdev，格式：<code>冗余级别: 盘1,盘2,...</code><br>' +
      "冗余级别：disk（单盘）/ mirror / raidz1 / raidz2 / raidz3</div>" +
      '<textarea id="pool-vdevs-input" rows="4" placeholder="mirror: /dev/sdb,/dev/sdc&#10;disk: /dev/sdd" ' +
      'style="width:100%;max-width:480px;padding:6px 8px;border:1px solid #d1d5db;border-radius:4px;font-family:monospace;"></textarea>' +
      "</div>" +
      '<div id="pool-form-msg" style="margin-bottom:8px;font-size:13px;"></div>' +
      '<div style="display:flex;gap:8px;">' +
      '<button type="button" id="pool-submit-btn" ' +
      'style="padding:6px 14px;background:#16a34a;color:#fff;border:none;border-radius:4px;cursor:pointer;">' +
      "提交</button>" +
      '<button type="button" id="pool-cancel-btn" ' +
      'style="padding:6px 14px;background:#fff;color:#374151;border:1px solid #d1d5db;border-radius:4px;cursor:pointer;">' +
      "取消</button>" +
      "</div>" +
      "</div>"
    );
  }

  /**
   * 解析用户输入的 vdevs 文本为 VdevSpec 数组。
   * 每行格式：`冗余级别: 盘1, 盘2, ...`（冗余级别：disk/mirror/raidz1/raidz2/raidz3）
   * @param {string} text
   * @returns {Array<{kind:string, disks:string[]}>}
   * @throws {Error} 解析失败
   */
  function parseVdevsInput(text) {
    const lines = text.split(/\r?\n/);
    const vdevs = [];
    const VALID = new Set(["disk", "mirror", "raidz1", "raidz2", "raidz3"]);
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i].trim();
      if (!line) continue;
      const idx = line.indexOf(":");
      if (idx === -1) {
        throw new Error("第 " + (i + 1) + " 行格式错误：缺少 ':' 分隔符");
      }
      const kind = line.slice(0, idx).trim().toLowerCase();
      const disksStr = line.slice(idx + 1).trim();
      if (!VALID.has(kind)) {
        throw new Error(
          "第 " + (i + 1) + " 行冗余级别非法：" + kind + "（应为 disk/mirror/raidz1/raidz2/raidz3）"
        );
      }
      const disks = disksStr
        .split(",")
        .map(function (s) {
          return s.trim();
        })
        .filter(function (s) {
          return s.length > 0;
        });
      if (!disks.length) {
        throw new Error("第 " + (i + 1) + " 行未提供磁盘路径");
      }
      vdevs.push({ kind: kind, disks: disks });
    }
    return vdevs;
  }

  async function submitCreatePool() {
    const msgEl = document.getElementById("pool-form-msg");
    const nameEl = document.getElementById("pool-name-input");
    const vdevsEl = document.getElementById("pool-vdevs-input");
    const submitBtn = document.getElementById("pool-submit-btn");
    if (!nameEl || !vdevsEl) return;

    const name = nameEl.value.trim();
    let vdevs;
    try {
      vdevs = parseVdevsInput(vdevsEl.value);
    } catch (e) {
      if (msgEl) {
        msgEl.style.color = "#b91c1c";
        msgEl.textContent = e.message;
      }
      return;
    }
    if (!name) {
      if (msgEl) {
        msgEl.style.color = "#b91c1c";
        msgEl.textContent = "请填写池名";
      }
      return;
    }
    if (!vdevs.length) {
      if (msgEl) {
        msgEl.style.color = "#b91c1c";
        msgEl.textContent = "请至少提供一个 vdev";
      }
      return;
    }

    if (submitBtn) submitBtn.disabled = true;
    if (msgEl) {
      msgEl.style.color = "#374151";
      msgEl.textContent = "创建中...";
    }
    try {
      await fetchJson("/api/v1/pools", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name: name, vdevs: vdevs }),
      });
      if (msgEl) {
        msgEl.style.color = "#15803d";
        msgEl.textContent = "创建成功";
      }
      // 收起表单 + 刷新列表
      const form = document.getElementById("storage-pool-form");
      if (form) {
        form.style.display = "none";
        form.innerHTML = "";
      }
      loadPoolsList();
    } catch (e) {
      if (msgEl) {
        msgEl.style.color = "#b91c1c";
        msgEl.textContent = "创建失败：" + (e.message || String(e));
      }
    } finally {
      if (submitBtn) submitBtn.disabled = false;
    }
  }

  // ===========================================================================
  // 数据集面板
  // ===========================================================================

  function renderDatasetsPanel(tc) {
    tc.innerHTML =
      '<div class="storage-datasets-panel">' +
      '<div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:12px;">' +
      "<h3 style=\"margin:0;\">数据集</h3>" +
      '<div style="display:flex;gap:8px;align-items:center;">' +
      '<label for="dataset-pool-filter" style="font-size:13px;color:#374151;">按池筛选：</label>' +
      '<select id="dataset-pool-filter" style="padding:5px 8px;border:1px solid #d1d5db;border-radius:4px;">' +
      '<option value="">（全部）</option>' +
      "</select>" +
      "</div>" +
      "</div>" +
      '<div id="storage-datasets-list">' +
      spinnerHtml("载数据集...") +
      "</div>" +
      "</div>";

    const filter = document.getElementById("dataset-pool-filter");
    if (filter) {
      filter.addEventListener("change", function () {
        loadDatasetsList(filter.value);
      });
    }
    // 先加载池列表填充筛选下拉，再加载数据集
    populatePoolFilter().then(loadDatasetsList);
  }

  async function populatePoolFilter() {
    const sel = document.getElementById("dataset-pool-filter");
    if (!sel) return;
    try {
      const pools = await fetchJson("/api/v1/pools");
      if (!Array.isArray(pools)) return;
      const cur = sel.value;
      // 保留第一项（全部）
      sel.innerHTML = '<option value="">（全部）</option>';
      pools.forEach(function (p) {
        const name = p.name || p.id;
        if (!name) return;
        const opt = document.createElement("option");
        opt.value = name;
        opt.textContent = name;
        sel.appendChild(opt);
      });
      sel.value = cur;
    } catch (_) {
      /* 筛选下拉加载失败不阻塞数据集列表 */
    }
  }

  async function loadDatasetsList(poolFilter) {
    const box = document.getElementById("storage-datasets-list");
    if (!box) return;
    box.innerHTML = spinnerHtml("载数据集...");
    try {
      const url = poolFilter
        ? "/api/v1/datasets?pool=" + encodeURIComponent(poolFilter)
        : "/api/v1/datasets";
      const datasets = await fetchJson(url);
      renderDatasetsTable(box, Array.isArray(datasets) ? datasets : []);
    } catch (e) {
      box.innerHTML = errorBoxHtml(e.message || String(e));
    }
  }

  function renderDatasetsTable(box, datasets) {
    if (!datasets.length) {
      box.innerHTML =
        '<div style="padding:24px;text-align:center;color:#666;">暂无数据集。</div>';
      return;
    }
    let html =
      '<table style="width:100%;border-collapse:collapse;font-size:14px;">' +
      "<thead><tr style=\"background:#f9fafb;text-align:left;\">" +
      '<th style="padding:10px;border-bottom:1px solid #e5e7eb;">名称</th>' +
      '<th style="padding:10px;border-bottom:1px solid #e5e7eb;">所属池</th>' +
      '<th style="padding:10px;border-bottom:1px solid #e5e7eb;">已用</th>' +
      '<th style="padding:10px;border-bottom:1px solid #e5e7eb;">可用</th>' +
      '<th style="padding:10px;border-bottom:1px solid #e5e7eb;">挂载</th>' +
      '<th style="padding:10px;border-bottom:1px solid #e5e7eb;">加密</th>' +
      "</tr></thead><tbody>";
    for (let i = 0; i < datasets.length; i++) {
      const d = datasets[i];
      const mounted = d.mounted;
      html +=
        "<tr>" +
        '<td style="padding:10px;border-bottom:1px solid #f3f4f6;font-weight:500;">' +
        escapeHtml(d.name || d.id) +
        "</td>" +
        '<td style="padding:10px;border-bottom:1px solid #f3f4f6;">' +
        escapeHtml(d.pool) +
        "</td>" +
        '<td style="padding:10px;border-bottom:1px solid #f3f4f6;">' +
        formatBytes(d.used_bytes) +
        "</td>" +
        '<td style="padding:10px;border-bottom:1px solid #f3f4f6;">' +
        formatBytes(d.avail_bytes) +
        "</td>" +
        '<td style="padding:10px;border-bottom:1px solid #f3f4f6;">' +
        mountBadge(mounted) +
        "</td>" +
        '<td style="padding:10px;border-bottom:1px solid #f3f4f6;">' +
        encryptionBadge(d.encryption) +
        "</td>" +
        "</tr>";
    }
    html += "</tbody></table>";
    box.innerHTML = html;
  }

  function mountBadge(mounted) {
    if (mounted) {
      return (
        '<span style="display:inline-block;padding:2px 10px;border-radius:10px;font-size:12px;' +
        'color:#15803d;background:#dcfce7;">已挂载</span>'
      );
    }
    return (
      '<span style="display:inline-block;padding:2px 10px;border-radius:10px;font-size:12px;' +
      'color:#6b7280;background:#f3f4f6;">未挂载</span>'
    );
  }

  // ===========================================================================
  // 快照面板
  // ===========================================================================

  function renderSnapshotsPanel(tc) {
    tc.innerHTML =
      '<div class="storage-snapshots-panel">' +
      '<div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:12px;">' +
      "<h3 style=\"margin:0;\">快照</h3>" +
      '<button type="button" id="storage-create-snap-btn" ' +
      'style="padding:6px 14px;background:#2563eb;color:#fff;border:none;border-radius:4px;cursor:pointer;">' +
      "+ 创建快照</button>" +
      "</div>" +
      '<div id="storage-snapshots-list">' +
      spinnerHtml("载快照...") +
      "</div>" +
      '<div id="storage-snap-form" style="display:none;"></div>' +
      "</div>";

    const createBtn = document.getElementById("storage-create-snap-btn");
    if (createBtn) {
      createBtn.addEventListener("click", function () {
        toggleCreateSnapshotForm();
      });
    }

    loadSnapshotsList();
  }

  async function loadSnapshotsList() {
    const box = document.getElementById("storage-snapshots-list");
    if (!box) return;
    box.innerHTML = spinnerHtml("载快照...");
    try {
      const snaps = await fetchJson("/api/v1/snapshots");
      renderSnapshotsTable(box, Array.isArray(snaps) ? snaps : []);
    } catch (e) {
      box.innerHTML = errorBoxHtml(e.message || String(e));
    }
  }

  function renderSnapshotsTable(box, snaps) {
    if (!snaps.length) {
      box.innerHTML =
        '<div style="padding:24px;text-align:center;color:#666;">暂无快照。</div>';
      return;
    }
    let html =
      '<table style="width:100%;border-collapse:collapse;font-size:14px;">' +
      "<thead><tr style=\"background:#f9fafb;text-align:left;\">" +
      '<th style="padding:10px;border-bottom:1px solid #e5e7eb;">名称</th>' +
      '<th style="padding:10px;border-bottom:1px solid #e5e7eb;">所属数据集</th>' +
      '<th style="padding:10px;border-bottom:1px solid #e5e7eb;">创建时间</th>' +
      '<th style="padding:10px;border-bottom:1px solid #e5e7eb;">大小</th>' +
      "</tr></thead><tbody>";
    for (let i = 0; i < snaps.length; i++) {
      const s = snaps[i];
      // created 是 RFC3339 字符串；直接显示，转本地时间
      const created = formatDateTime(s.created);
      html +=
        "<tr>" +
        '<td style="padding:10px;border-bottom:1px solid #f3f4f6;font-weight:500;">' +
        escapeHtml(s.id) +
        "</td>" +
        '<td style="padding:10px;border-bottom:1px solid #f3f4f6;">' +
        escapeHtml(s.dataset) +
        "</td>" +
        '<td style="padding:10px;border-bottom:1px solid #f3f4f6;">' +
        escapeHtml(created) +
        "</td>" +
        '<td style="padding:10px;border-bottom:1px solid #f3f4f6;">' +
        formatBytes(s.used_bytes) +
        "</td>" +
        "</tr>";
    }
    html += "</tbody></table>";
    box.innerHTML = html;
  }

  /**
   * 把 RFC3339 时间字符串格式化为本地可读时间；解析失败原样返回。
   * @param {string} iso
   * @returns {string}
   */
  function formatDateTime(iso) {
    if (!iso) return "-";
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    // YYYY-MM-DD HH:mm:ss
    const pad = function (n) {
      return n < 10 ? "0" + n : String(n);
    };
    return (
      d.getFullYear() +
      "-" +
      pad(d.getMonth() + 1) +
      "-" +
      pad(d.getDate()) +
      " " +
      pad(d.getHours()) +
      ":" +
      pad(d.getMinutes()) +
      ":" +
      pad(d.getSeconds())
    );
  }

  /**
   * 切换显示/隐藏创建快照表单。
   * 后端目前仅声明 GET /api/v1/snapshots（无 POST 快照路由），故「创建快照」
   * 按钮提交时若后端返回 404/405，会在表单内提示「后端暂不支持创建快照」。
   */
  function toggleCreateSnapshotForm() {
    const form = document.getElementById("storage-snap-form");
    if (!form) return;
    if (form.style.display === "none") {
      form.style.display = "block";
      form.innerHTML = buildCreateSnapshotFormHtml();
      const cancelBtn = document.getElementById("snap-cancel-btn");
      const submitBtn = document.getElementById("snap-submit-btn");
      if (cancelBtn) {
        cancelBtn.addEventListener("click", function () {
          form.style.display = "none";
          form.innerHTML = "";
        });
      }
      if (submitBtn) {
        submitBtn.addEventListener("click", submitCreateSnapshot);
      }
    } else {
      form.style.display = "none";
      form.innerHTML = "";
    }
  }

  function buildCreateSnapshotFormHtml() {
    return (
      '<div style="margin-top:12px;padding:16px;border:1px solid #e5e7eb;border-radius:6px;background:#fafafa;">' +
      '<h4 style="margin:0 0 12px 0;">创建快照</h4>' +
      '<div style="margin-bottom:12px;">' +
      '<label style="display:block;margin-bottom:4px;font-size:13px;">数据集名</label>' +
      '<input type="text" id="snap-dataset-input" placeholder="例如 tank/media" ' +
      'style="width:100%;max-width:320px;padding:6px 8px;border:1px solid #d1d5db;border-radius:4px;" />' +
      "</div>" +
      '<div style="margin-bottom:12px;">' +
      '<label style="display:block;margin-bottom:4px;font-size:13px;">快照名</label>' +
      '<input type="text" id="snap-name-input" placeholder="例如 snap1" ' +
      'style="width:100%;max-width:320px;padding:6px 8px;border:1px solid #d1d5db;border-radius:4px;" />' +
      "</div>" +
      '<div id="snap-form-msg" style="margin-bottom:8px;font-size:13px;"></div>' +
      '<div style="display:flex;gap:8px;">' +
      '<button type="button" id="snap-submit-btn" ' +
      'style="padding:6px 14px;background:#16a34a;color:#fff;border:none;border-radius:4px;cursor:pointer;">' +
      "提交</button>" +
      '<button type="button" id="snap-cancel-btn" ' +
      'style="padding:6px 14px;background:#fff;color:#374151;border:1px solid #d1d5db;border-radius:4px;cursor:pointer;">' +
      "取消</button>" +
      "</div>" +
      "</div>"
    );
  }

  async function submitCreateSnapshot() {
    const msgEl = document.getElementById("snap-form-msg");
    const dsEl = document.getElementById("snap-dataset-input");
    const nameEl = document.getElementById("snap-name-input");
    const submitBtn = document.getElementById("snap-submit-btn");
    if (!dsEl || !nameEl) return;

    const dataset = dsEl.value.trim();
    const snapName = nameEl.value.trim();
    if (!dataset) {
      if (msgEl) {
        msgEl.style.color = "#b91c1c";
        msgEl.textContent = "请填写数据集名";
      }
      return;
    }
    if (!snapName) {
      if (msgEl) {
        msgEl.style.color = "#b91c1c";
        msgEl.textContent = "请填写快照名";
      }
      return;
    }
    if (submitBtn) submitBtn.disabled = true;
    if (msgEl) {
      msgEl.style.color = "#374151";
      msgEl.textContent = "创建中...";
    }
    try {
      // 后端当前路由表未声明 POST /api/v1/snapshots；此处按 RESTful 约定尝试，
      // 失败（404/405）会在 catch 里给出可读提示，不抛坏页面。
      await fetchJson("/api/v1/snapshots", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ dataset: dataset, name: snapName }),
      });
      if (msgEl) {
        msgEl.style.color = "#15803d";
        msgEl.textContent = "创建成功";
      }
      const form = document.getElementById("storage-snap-form");
      if (form) {
        form.style.display = "none";
        form.innerHTML = "";
      }
      loadSnapshotsList();
    } catch (e) {
      const m = e.message || String(e);
      // 后端未实现该路由时给出更友好的提示
      const friendly = /404|405|未匹配|not found|method not allowed/i.test(m)
        ? "后端暂不支持创建快照（未实现 POST /api/v1/snapshots）"
        : "创建失败：" + m;
      if (msgEl) {
        msgEl.style.color = "#b91c1c";
        msgEl.textContent = friendly;
      }
    } finally {
      if (submitBtn) submitBtn.disabled = false;
    }
  }

  // ===========================================================================
  // 导出
  // ===========================================================================

  const storageModule = {
    loadStoragePage: loadStoragePage,
    // 暴露工具函数，便于其他模块复用 / 单测
    formatBytes: formatBytes,
    escapeHtml: escapeHtml,
    fetchJson: fetchJson,
  };

  global.StorageUI = storageModule;
  // loadStoragePage 也挂到 window 顶层，方便 index.html 直接 onclick 调用
  global.loadStoragePage = loadStoragePage;

  // 兼容 CommonJS（node 单测 node -c 只查语法；此处不影响）
  if (typeof module !== "undefined" && module.exports) {
    module.exports = storageModule;
  }
})(typeof window !== "undefined" ? window : this);
