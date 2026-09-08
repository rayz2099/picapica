import { applyI18n, resolveUiLang, t } from "./i18n.js";

const THEME_KEY = "picapica.theme";
const LANG_KEY = "picapica.lang";
const TOKEN_KEY = "picapica_token";
const TABS = ["dash", "repos", "probe", "downloads", "settings"];

function $(id) {
  return document.getElementById(id);
}

/** why: 动态节点一律 DOM 拼接，避免用户数据进 innerHTML。 */
function el(tag, props, ...kids) {
  const node = document.createElement(tag);
  if (props) {
    for (const [key, val] of Object.entries(props)) {
      if (val == null || val === false) continue;
      if (key === "className") node.className = val;
      else if (key === "text") node.textContent = val;
      else if (key === "dataset") Object.assign(node.dataset, val);
      else if (key === "checked" || key === "hidden" || key === "disabled" || key === "readOnly") node[key] = val;
      else if (key.startsWith("on") && typeof val === "function") node.addEventListener(key.slice(2).toLowerCase(), val);
      else node.setAttribute(key, val);
    }
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    node.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return node;
}

function clear(node) {
  node.replaceChildren();
}
/** why: 轮询只改文本，避免清 DOM 造成闪屏。 */
function setText(node, text) {
  const next = String(text);
  if (node.textContent !== next) node.textContent = next;
}

function themeMode() {
  const v = localStorage.getItem(THEME_KEY);
  if (v === "light" || v === "dark") return v;
  return "system";
}

function langPref() {
  return localStorage.getItem(LANG_KEY) || "system";
}

/** why: 首屏 inline 脚本只挡闪白；这里跟系统偏好保持同步。 */
function applyTheme(mode) {
  const next = mode || "system";
  const dark = next === "dark" || (next !== "light" && matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.classList.toggle("dark", dark);
  document.documentElement.dataset.theme = next;
  localStorage.setItem(THEME_KEY, next);
  document.querySelectorAll("theme-toggle").forEach((n) => n.sync());
  document.querySelectorAll("#theme-seg button").forEach((b) => {
    b.classList.toggle("on", b.dataset.theme === next);
  });
}

function cycleTheme() {
  const order = ["system", "light", "dark"];
  const cur = themeMode();
  applyTheme(order[(order.indexOf(cur) + 1) % order.length]);
}

const ICONS = {
  system: '<svg viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="5" width="18" height="12" rx="2" fill="none" stroke="currentColor" stroke-width="1.8"/><path d="M8 19h8" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/></svg>',
  light: '<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="4" fill="none" stroke="currentColor" stroke-width="1.8"/><path d="M12 3v2m0 14v2m9-9h-2M5 12H3m13.5-6.5-1.4 1.4M8.9 15.1 7.5 16.5m8.6 0-1.4-1.4M8.9 8.9 7.5 7.5" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/></svg>',
  dark: '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M15 4.5A7.5 7.5 0 1 0 19.5 15 6 6 0 0 1 15 4.5z" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"/></svg>',
};

/** why: 登录页和侧栏共用同一套三态，避免两处主题状态分叉。 */
class ThemeToggle extends HTMLElement {
  connectedCallback() {
    this.btn = el("button", { type: "button", className: "icon-only", onClick: () => cycleTheme() });
    this.replaceChildren(this.btn);
    this.sync();
  }
  sync() {
    if (!this.btn) return;
    const mode = themeMode();
    this.btn.innerHTML = ICONS[mode];
    this.btn.title = t(langPref(), mode === "light" ? "themeLight" : mode === "dark" ? "themeDark" : "themeSystem");
  }
}
customElements.define("theme-toggle", ThemeToggle);

function fmtBytes(n) {
  const x = Number(n) || 0;
  if (x < 1024) return x + " B";
  if (x < 1048576) return (x / 1024).toFixed(1) + " KB";
  if (x < 1073741824) return (x / 1048576).toFixed(1) + " MB";
  return (x / 1073741824).toFixed(2) + " GB";
}

function fmtSpeed(n) {
  const value = Number.isFinite(n) && n > 0 ? n : 0;
  return (value / 1048576).toFixed(2) + " MB/s";
}

function artifactLabel(row) {
  const ns = String(row.namespace || "").replace(/^\/+|\/+$/g, "");
  const name = String(row.name || "").replace(/^\/+|\/+$/g, "");
  // why: HTTP 文件的 name 已是完整路径，不能再重复拼接其 namespace 前缀。
  if (ns && name && name !== ns && !name.startsWith(ns + "/")) return ns + "/" + name;
  return name || ns || "—";
}

/** why: API 已收敛为安全主机字段，原样显示可避免前端重新解释上游地址。 */
function upstreamHost(value) {
  return String(value || "—");
}

/** why: 控制面所有请求共用令牌；401 必须回到闸门，不能换一条匿名路径。 */
class Api {
  constructor() {
    this.token = localStorage.getItem(TOKEN_KEY) || "";
  }
  headers() {
    return { Authorization: "Bearer " + this.token, "Content-Type": "application/json" };
  }
  async call(path, opt) {
    const res = await fetch(path, Object.assign({ headers: this.headers() }, opt || {}));
    if (res.status === 401) {
      const err = new Error("unauthorized");
      err.code = 401;
      throw err;
    }
    if (!res.ok) throw new Error(await res.text());
    const text = await res.text();
    return text ? JSON.parse(text) : {};
  }

  /** why: 健康接口用非 2xx 表示降级，UI 仍需读取正文展示具体故障。 */
  async inspect(path) {
    const res = await fetch(path, { headers: this.headers() });
    const text = await res.text();
    const data = text ? JSON.parse(text) : {};
    data.http_ok = res.ok;
    return data;
  }
}

/** why: 控制面是单页状态机，闸门、路由、各页渲染分开，避免旧脚本全局变量互相踩。 */
class App {
  constructor() {
    this.api = new Api();
    this.cfg = null;
    this.ranks = {};
    this.treeRepo = "";
    this.treePrefix = "";
    this.treePage = 1;
    this.treeData = null;
    this.tabId = "dash";
    this.stats = { artifacts: 0, bytes: 0, refs: 0, repos: 0 };
    this.health = { ok: false };
    this.transfers = [];
    this.transferError = "";
    this.confirmWait = null;
    this.toastTimer = 0;
    this.lastCfgJson = "";
    this.pollTimer = 0;
    this.polling = false;
    this.transferPollTimer = 0;
    this.transferPolling = false;
    this.dashSnap = "";
    this.xferSnap = "";
  }

  tx(key, vars) {
    return t(langPref(), key, vars);
  }

  fail(err) {
    if (err.code === 401) {
      this.showGate(this.tx("tokenInvalid"));
      return;
    }
    this.toast(err.message);
  }

  boot() {
    applyTheme(themeMode());
    this.paintI18n();
    $("lang-select").value = langPref();
    matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
      if (themeMode() === "system") applyTheme("system");
    });
    window.addEventListener("languagechange", () => {
      if (langPref() !== "system") return;
      this.paintI18n();
      this.renderAll();
    });
    document.querySelectorAll(".nav button").forEach((btn) => {
      btn.addEventListener("click", () => this.go(btn.dataset.tab));
    });
    document.querySelectorAll("#theme-seg button").forEach((btn) => {
      btn.addEventListener("click", () => applyTheme(btn.dataset.theme));
    });
    $("lang-select").addEventListener("change", () => {
      localStorage.setItem(LANG_KEY, $("lang-select").value);
      this.paintI18n();
      this.renderAll();
    });
    $("gate-form").addEventListener("submit", (ev) => {
      ev.preventDefault();
      this.enter($("token-input").value.trim());
    });
    $("btn-add-repo").addEventListener("click", () => this.openDrawer(null));
    $("btn-cancel").addEventListener("click", () => { $("drawer").hidden = true; });
    $("repo-form").addEventListener("submit", (ev) => {
      ev.preventDefault();
      this.saveRepo();
    });
    $("btn-probe").addEventListener("click", () => this.runProbe());
    $("btn-prune-plan").addEventListener("click", () => this.prune(true));
    $("btn-prune-run").addEventListener("click", () => this.prune(false));
    $("btn-discard").addEventListener("click", () => this.discardSettings());
    $("btn-save").addEventListener("click", () => this.saveSettings());
    $("chk-all").addEventListener("change", (ev) => {
      document.querySelectorAll(".ns-chk").forEach((box) => { box.checked = ev.target.checked; });
    });
    $("btn-del-sel").addEventListener("click", () => this.delSelected());
    $("btn-del-all").addEventListener("click", () => this.delLevel());
    $("confirm-cancel").addEventListener("click", () => this.endConfirm(false));
    $("confirm-ok").addEventListener("click", () => this.endConfirm(true));
    $("drawer").addEventListener("click", (ev) => {
      if (ev.target === $("drawer")) $("drawer").hidden = true;
    });
    $("confirm").addEventListener("click", (ev) => {
      if (ev.target === $("confirm")) this.endConfirm(false);
    });
    window.addEventListener("hashchange", () => this.syncHash());
    if (!this.api.token) {
      this.showGate("");
      return;
    }
    this.enter(this.api.token);
  }

  paintI18n() {
    applyI18n(document, langPref());
    applyTheme(themeMode());
  }

  showGate(msg) {
    const root = document.documentElement;
    root.classList.add("pica-gate");
    root.classList.remove("pica-boot", "pica-ready");
    const err = $("gate-err");
    err.hidden = !msg;
    err.textContent = msg || "";
  }

  async enter(token) {
    this.api.token = token;
    localStorage.setItem(TOKEN_KEY, token);
    try {
      await this.refresh();
      await this.refreshTransfers();
      const root = document.documentElement;
      root.classList.remove("pica-gate", "pica-boot");
      root.classList.add("pica-ready");
      $("gate-err").hidden = true;
      this.syncHash();
      clearInterval(this.pollTimer);
      this.pollTimer = setInterval(() => this.refreshRuntime(), 5000);
      clearInterval(this.transferPollTimer);
      this.transferPollTimer = setInterval(() => this.refreshTransfers(), 1000);
    } catch (e) {
      this.showGate(e.code === 401 ? this.tx("tokenInvalid") : e.message);
    }
  }

  go(tab, repo, prefix) {
    const bits = [tab || "dash"];
    if (tab === "repos" && repo) {
      bits.push(encodeURIComponent(repo));
      if (prefix) prefix.split("/").filter(Boolean).forEach((p) => bits.push(encodeURIComponent(p)));
    }
    location.hash = "#/" + bits.join("/");
  }

  parseHash() {
    const raw = location.hash.replace(/^#\/?/, "");
    const parts = raw.split("/").filter(Boolean).map((s) => decodeURIComponent(s));
    const tab = TABS.includes(parts[0]) ? parts[0] : "dash";
    const repo = tab === "repos" ? (parts[1] || "") : "";
    const prefix = tab === "repos" ? parts.slice(2).join("/") : "";
    return { tab, repo, prefix };
  }

  async syncHash() {
    if (!this.cfg) return;
    const { tab, repo, prefix } = this.parseHash();
    this.showTab(tab);
    try {
      if (tab === "repos" && repo) {
        await this.openTree(repo, prefix, 1);
        return;
      }
      if (tab === "repos") this.showRepoHome();
    } catch (e) {
      this.fail(e);
    }
  }

  showTab(id) {
    this.tabId = id;
    document.querySelectorAll("main > section").forEach((sec) => { sec.hidden = sec.id !== id; });
    document.querySelectorAll(".nav button").forEach((btn) => {
      btn.classList.toggle("active", btn.dataset.tab === id);
    });
    const titles = {
      dash: ["overview", "overviewLede"],
      repos: ["repos", "reposLede"],
      probe: ["probe", "probeLede"],
      downloads: ["downloads", "downloadsLede"],
      settings: ["settings", "settingsLede"],
    };
    const pair = titles[id];
    $("title").textContent = this.tx(pair[0]);
    $("lede").textContent = this.tx(pair[1]);
    if (id === "downloads") {
      this.renderTransfers();
      this.refreshTransfers();
    }
  }

  async refresh() {
    const [st, next, ranks, health] = await Promise.all([
      this.api.call("/api/stats"),
      this.api.call("/api/config"),
      this.api.call("/api/probe"),
      this.api.inspect("/api/health"),
    ]);
    this.cfg = next;
    this.ranks = ranks;
    this.stats = st;
    this.health = health;
    this.fillSettings();
    this.renderDash(st, health);
    this.renderProbe(ranks);
    if (this.tabId === "repos" && !this.treeRepo) this.showRepoHome();
  }

  /** why: 命中统计是运行态，短轮询只取轻量接口，不反复覆盖正在编辑的配置。 */
  async refreshRuntime() {
    if (this.polling || !this.cfg) return;
    this.polling = true;
    try {
      const [st, health] = await Promise.all([
        this.api.call("/api/stats"),
        this.api.inspect("/api/health"),
      ]);
      this.stats = st;
      this.health = health;
      if (this.tabId === "dash") this.renderDash(st, health);
    } catch (e) {
      if (e.code === 401) this.showGate(this.tx("tokenInvalid"));
      this.health = { ok: false, http_ok: false, error: e.message };
      if (this.tabId === "dash") this.renderDash(this.stats, this.health);
    } finally {
      this.polling = false;
    }
  }

  renderAll() {
    if (!this.cfg) return;
    this.showTab(this.tabId);
    this.renderDash(this.stats, this.health);
    this.renderProbe(this.ranks);
    this.renderTransfers();
    if (this.tabId === "repos") {
      if (this.treeRepo && this.treeData) this.paintTree();
      else this.showRepoHome();
    }
    this.fillSettings();
    document.querySelectorAll("theme-toggle").forEach((n) => n.sync());
  }

  renderDash(st, health) {
    const root = $("stats");
    const cacheLabel = this.cfg.cache ? this.tx("cacheOn") : this.tx("cacheOff");
    const cards = [
      [this.tx("artifacts"), st.artifacts],
      [this.tx("bytes"), fmtBytes(st.bytes)],
      [this.tx("refs"), st.refs],
      [this.tx("repos"), st.repos],
      [this.tx("cache"), cacheLabel],
      [this.tx("cacheHits"), st.cache_hits || 0],
      [this.tx("cacheMisses"), st.cache_misses || 0],
      [this.tx("upstreamFailures"), st.upstream_failures || 0],
      [this.tx("activeTransfers"), st.active_transfers || 0],
    ];
    const snap = JSON.stringify({ cards, health, ranks: this.ranks, repos: this.cfg.repos, lang: langPref() });
    if (snap === this.dashSnap) return;
    this.dashSnap = snap;
    if (root.childElementCount === cards.length) {
      cards.forEach((card, i) => {
        setText(root.children[i].querySelector("b"), card[0]);
        setText(root.children[i].querySelector("em"), card[1]);
      });
    } else {
      clear(root);
      for (const [k, v] of cards) {
        root.append(el("div", { className: "stat" }, el("b", { text: k }), el("em", { text: String(v) })));
      }
    }
    const healthNode = $("health-state");
    const healthy = Boolean(health && health.ok && health.http_ok);
    healthNode.className = "health-state " + (healthy ? "healthy" : "unhealthy");
    setText(healthNode, healthy
      ? this.tx("healthy")
      : this.tx("unhealthy") + (health && health.error ? " · " + health.error : ""));
    const body = $("dash-repos");
    const repos = this.cfg.repos;
    if (body.childElementCount === repos.length) {
      repos.forEach((repo, i) => {
        const best = (this.ranks[repo.name] || []).find((row) => row.ok);
        const tds = body.children[i].children;
        setText(tds[0], repo.name);
        setText(tds[1], repo.type);
        setText(tds[2], repo.upstreams.length);
        if (best) setText(tds[3], best.url + " " + best.rtt_ms + "ms");
        else setText(tds[3], this.tx("none"));
      });
      return;
    }
    clear(body);
    for (const repo of repos) {
      const best = (this.ranks[repo.name] || []).find((row) => row.ok);
      const fast = best
        ? el("td", {}, best.url + " ", el("span", { className: "ok", text: best.rtt_ms + "ms" }))
        : el("td", { text: this.tx("none") });
      body.append(el("tr", {},
        el("td", { text: repo.name }),
        el("td", { text: repo.type }),
        el("td", { text: String(repo.upstreams.length) }),
        fast,
      ));
    }
  }

  /** why: 下载进度变化快，独立轮询避免拖慢总览运行态，也不覆盖正在编辑的配置。 */
  async refreshTransfers() {
    if (this.transferPolling || !this.cfg) return;
    this.transferPolling = true;
    try {
      const result = await this.api.call("/api/transfers");
      const rows = Array.isArray(result) ? result : [];
      this.transfers = rows;
      this.transferError = "";
      this.renderTransfers();
    } catch (e) {
      if (e.code === 401) this.showGate(this.tx("tokenInvalid"));
      this.transferError = e.message;
      this.renderTransfers();
    } finally {
      this.transferPolling = false;
    }
  }

  renderTransfers() {
    const body = $("transfer-body");
    const summary = $("transfer-summary");
    const snap = JSON.stringify({
      err: this.transferError,
      rows: this.transfers,
      lang: langPref(),
    });
    if (snap === this.xferSnap) return;
    this.xferSnap = snap;
    if (this.transferError) {
      summary.className = "transfer-summary bad";
      setText(summary, this.tx("downloadProgressError", { error: this.transferError }));
      clear(body);
      const cell = el("td", { className: "empty", colspan: "6", text: this.tx("downloadProgressUnavailable") });
      body.append(el("tr", {}, cell));
      return;
    }
    const totalSpeed = this.transfers.reduce((sum, row) => sum + (Number(row.speed_bps) || 0), 0);
    summary.className = "transfer-summary";
    setText(summary, this.tx("downloadSummary", {
      n: String(this.transfers.length),
      speed: fmtSpeed(totalSpeed),
    }));
    if (!this.transfers.length) {
      clear(body);
      const cell = el("td", { className: "empty", colspan: "6", text: this.tx("noDownloads") });
      body.append(el("tr", {}, cell));
      return;
    }
    const ids = this.transfers.map((row) => String(row.id));
    const same = body.childElementCount === ids.length
      && [...body.children].every((tr, i) => tr.dataset.id === ids[i]);
    if (same) {
      this.transfers.forEach((row, i) => this.paintXfer(body.children[i], row));
      return;
    }
    clear(body);
    for (const row of this.transfers) {
      const tr = el("tr", { dataset: { id: String(row.id) } });
      tr.append(
        el("td", { dataset: { label: this.tx("repo") } }),
        el("td", { className: "transfer-artifact", dataset: { label: this.tx("artifact") } }),
        el("td", { className: "transfer-host", dataset: { label: this.tx("upstreamHost") } }),
        el("td", { className: "number", dataset: { label: this.tx("downloaded") } }),
        el("td", { dataset: { label: this.tx("progress") } }),
        el("td", { className: "number", dataset: { label: this.tx("speed") } }),
      );
      this.paintXfer(tr, row);
      body.append(tr);
    }
  }

  /** why: 进度每秒变，整表重建会闪；同一行只改数字和 scaleX。 */
  paintXfer(tr, row) {
    const tds = tr.children;
    const received = Number(row.received) || 0;
    const total = Number(row.total) || 0;
    const attempted = Number(row.attempt_received) || 0;
    const pct = total > 0 ? Math.min(100, Math.round(attempted * 100 / total)) : null;
    setText(tds[0], row.repo);
    setText(tds[1], artifactLabel(row));
    setText(tds[2], upstreamHost(row.upstream));
    setText(tds[3], fmtBytes(received));
    setText(tds[5], fmtSpeed(Number(row.speed_bps) || 0));
    if (pct == null) {
      if (!tds[4].querySelector(".progress-cell")) setText(tds[4], "—");
      else tds[4].replaceChildren(el("span", { className: "number", text: "—" }));
      return;
    }
    let fill = tds[4].querySelector(".transfer-bar > i");
    if (!fill) {
      fill = el("i");
      const bar = el("span", {
        className: "bar transfer-bar",
        role: "progressbar",
        "aria-label": this.tx("progress"),
        "aria-valuemin": "0",
        "aria-valuemax": "100",
      }, fill);
      tds[4].replaceChildren(el("div", { className: "progress-cell" }, el("span", { className: "number" }), bar));
    }
    const bar = tds[4].querySelector(".transfer-bar");
    bar.setAttribute("aria-valuenow", String(pct));
    setText(tds[4].querySelector(".number"), pct + "%");
    fill.style.setProperty("--p", String(pct / 100));
  }

  async prune(dryRun) {
    if (!dryRun && !await this.ask(this.tx("pruneConfirm"))) return;
    const msg = $("prune-msg");
    msg.textContent = this.tx("pruning");
    $("btn-prune-plan").disabled = true;
    $("btn-prune-run").disabled = true;
    try {
      const report = await this.api.call("/api/cache/prune?dry_run=" + String(dryRun), { method: "POST" });
      msg.textContent = this.tx(dryRun ? "prunePreviewResult" : "pruneResult", {
        artifacts: String(report.artifacts || 0),
        bytes: fmtBytes(report.bytes_reclaimable || 0),
      });
      await this.refresh();
    } catch (e) {
      this.fail(e);
      msg.textContent = e.message;
    } finally {
      $("btn-prune-plan").disabled = false;
      $("btn-prune-run").disabled = false;
    }
  }

  listenHost() {
    return (this.cfg.listen || "127.0.0.1:8080").replace("0.0.0.0", "127.0.0.1");
  }

  publicBase() {
    const raw = (this.cfg.public_url || "").trim().replace(/\/$/, "");
    return raw || ("http://" + this.listenHost());
  }

  dockerHost(wantTls) {
    const raw = (this.cfg.public_url || "").trim();
    if (raw) {
      const parsed = new URL(raw);
      if (wantTls && parsed.protocol === "https:") return parsed.host;
      if (!wantTls) return parsed.host;
    }
    if (wantTls) return "pica.example.com";
    return this.listenHost();
  }

  /** why: 复制的是可粘贴执行的脚本，不是带围栏的 markdown。 */
  fence(body) {
    const copy = el("button", { type: "button", className: "ghost", text: this.tx("copy") });
    copy.addEventListener("click", () => this.copyText(body, copy));
    return el("div", { className: "fence" },
      el("div", { className: "fence-bar" },
        el("span", { className: "fence-lang", text: "```shell" }),
        copy,
      ),
      el("pre", { text: body }),
    );
  }

  /** why: 卡片默认只留操作；教程给可整段替换的 shell，避免手工改 json。 */
  paintGuide(repo) {
    const pathBase = this.publicBase() + "/" + repo.name;
    const httpHost = this.dockerHost(false);
    const inner = el("div", { className: "guide-body" });
    const name = repo.name;
    if (repo.type === "docker") {
      const daemon = JSON.stringify({
        "insecure-registries": [httpHost],
        "registry-mirrors": ["http://" + httpHost],
      }, null, 2);
      const setup = [
        "cat > /etc/docker/daemon.json <<'EOF'",
        daemon,
        "EOF",
      ].join("\n");
      const pull = "docker pull " + httpHost + "/" + name + "/library/busybox";
      inner.append(
        el("p", { text: this.tx("tutDocker") }),
        this.fence(setup),
        this.fence(pull),
      );
    } else if (repo.type === "ubuntu") {
      const setup = [
        "cat > /etc/apt/sources.list.d/" + name + ".list <<'EOF'",
        "deb " + pathBase + " jammy main",
        "EOF",
        "apt-get update",
      ].join("\n");
      inner.append(el("p", { text: this.tx("tutApt") }), this.fence(setup));
    } else {
      const setup = [
        "cat > /etc/yum.repos.d/" + name + ".repo <<'EOF'",
        "[" + name + "]",
        "name=" + name,
        "baseurl=" + pathBase + "/releases/$releasever/Everything/$basearch/os/",
        "enabled=1",
        "gpgcheck=0",
        "EOF",
        "dnf makecache",
      ].join("\n");
      inner.append(el("p", { text: this.tx("tutDnf") }), this.fence(setup));
    }
    return el("details", { className: "guide" },
      el("summary", { text: this.tx("tutorial") }),
      inner,
    );
  }

  async copyText(text, btn) {
    try {
      await navigator.clipboard.writeText(text);
    } catch (e) {
      this.fail(e);
      return;
    }
    if (btn) {
      const prev = btn.textContent;
      btn.textContent = this.tx("copied");
      setTimeout(() => { btn.textContent = prev; }, 1200);
    }
    this.toast(this.tx("copied"));
  }

  showRepoHome() {
    this.treeRepo = "";
    $("repo-home").hidden = false;
    $("tree-view").hidden = true;
    const root = $("repo-home");
    clear(root);
    const repos = this.cfg.repos || [];
    if (!repos.length) {
      root.append(el("p", { className: "empty", text: this.tx("emptyRepos") }));
      return;
    }
    repos.forEach((repo) => {
      const alias = (repo.aliases || []).join(", ") || this.tx("none");
      const meta = this.tx("upstreamN", { n: String(repo.upstreams.length) }) + " · " + this.tx("aliasN", { list: alias });
      const tools = el("div", { className: "toolbar" });
      const copyPath = el("button", { type: "button", className: "ghost", text: this.tx("copyPath") });
      copyPath.addEventListener("click", () => this.copyText(this.publicBase() + "/" + repo.name, copyPath));
      tools.append(copyPath);
      if (this.cfg.cache) {
        const enter = el("button", { type: "button", className: "pill", text: this.tx("enterTree") });
        enter.addEventListener("click", () => this.go("repos", repo.name, ""));
        tools.append(enter);
      }
      const edit = el("button", { type: "button", className: "ghost", text: this.tx("edit") });
      edit.addEventListener("click", () => this.openDrawer(repo));
      const rm = el("button", { type: "button", className: "danger", text: this.tx("deleteRepo") });
      rm.addEventListener("click", () => this.removeRepo(repo));
      tools.append(edit, rm);

      root.append(el("article", { className: "repo" },
        el("div", { className: "toolbar" },
          el("h3", { text: repo.name }),
          el("span", { className: "chip", text: repo.type }),
        ),
        el("div", { className: "meta", text: meta }),
        tools,
        this.paintGuide(repo),
      ));
    });
  }

  async openTree(repo, prefix, page) {
    this.treeRepo = repo;
    this.treePrefix = prefix;
    this.treePage = page;
    $("repo-home").hidden = true;
    $("tree-view").hidden = false;
    const q = "/api/tree?repo=" + encodeURIComponent(repo) + "&prefix=" + encodeURIComponent(prefix) + "&page=" + page;
    this.treeData = await this.api.call(q);
    this.paintTree();
  }

  paintTree() {
    const repo = this.treeRepo;
    const prefix = this.treePrefix;
    const crumb = $("crumb");
    clear(crumb);
    const rootBtn = el("button", { type: "button", text: repo });
    rootBtn.addEventListener("click", () => this.go("repos", repo, ""));
    crumb.append(rootBtn);
    let acc = "";
    prefix.split("/").filter(Boolean).forEach((part) => {
      acc = acc ? acc + "/" + part : part;
      const path = acc;
      crumb.append(el("span", { className: "sep", text: "/" }));
      const btn = el("button", { type: "button", text: part });
      btn.addEventListener("click", () => this.go("repos", repo, path));
      crumb.append(btn);
    });
    const body = $("tree-body");
    clear(body);
    const rows = [];
    if (this.treeData.self_ns) rows.push([this.treeData.self_ns, true]);
    for (const entry of this.treeData.entries || []) rows.push([entry, false]);
    if (!rows.length) {
      body.append(el("tr", {}, el("td", { className: "empty", colspan: "6", text: this.tx("emptyTree") })));
    } else {
      for (const [entry, isSelf] of rows) body.append(this.treeRow(entry, isSelf));
    }
    const totalPages = Math.max(1, Math.ceil((this.treeData.total || 0) / (this.treeData.per_page || 50)));
    const pager = $("pager");
    clear(pager);
    pager.append(el("span", { className: "muted", text: this.tx("items", { n: String(this.treeData.total || 0) }) }));
    const prev = el("button", { type: "button", className: "ghost", text: this.tx("prev") });
    const next = el("button", { type: "button", className: "ghost", text: this.tx("next") });
    prev.addEventListener("click", () => {
      if (this.treePage > 1) this.openTree(repo, prefix, this.treePage - 1);
    });
    next.addEventListener("click", () => {
      if (this.treePage < totalPages) this.openTree(repo, prefix, this.treePage + 1);
    });
    pager.append(prev, el("span", { text: this.treePage + " / " + totalPages }), next);
    $("chk-all").checked = false;
  }

  treeRow(entry, isSelf) {
    const name = (isSelf ? this.tx("thisLevel") + " · " : "") + entry.name;
    const chips = el("div", { className: "chip-list" });
    for (const tag of entry.tags || []) {
      chips.append(el("span", { className: "chip-item" },
        el("span", { className: "chip", text: tag }),
        el("button", { type: "button", className: "chip-del", title: this.tx("delTag", { ns: entry.namespace, tag }), "aria-label": this.tx("removeTag", { tag }), onClick: () => this.delTag(entry.namespace, tag) }, "×")));
    }
    const tools = el("div", { className: "row-actions" });
    if (entry.deeper || (!entry.leaf && entry.namespace)) {
      tools.append(el("button", { type: "button", className: "ghost", text: this.tx("open"), onClick: () => this.go("repos", this.treeRepo, entry.namespace) }));
    }
    let chk = el("td", { className: "check" });
    if (entry.leaf) {
      chk = el("td", { className: "check" }, el("input", { type: "checkbox", className: "ns-chk", value: entry.namespace }));
      tools.append(el("button", { type: "button", className: "danger", text: this.tx("delete"), onClick: () => this.delNs(entry.namespace) }));
    }
    return el("tr", {},
      chk,
      el("td", { className: "name-cell" }, el("span", { className: "path", text: name })),
      el("td", { className: "number", text: String(entry.objects) }),
      el("td", { className: "number", text: fmtBytes(entry.bytes) }),
      el("td", {}, chips),
      el("td", {}, tools),
    );
  }

  async ask(body) {
    $("confirm-body").textContent = body;
    $("confirm").hidden = false;
    return new Promise((resolve) => { this.confirmWait = resolve; });
  }

  endConfirm(ok) {
    $("confirm").hidden = true;
    const wait = this.confirmWait;
    this.confirmWait = null;
    if (wait) wait(ok);
  }

  toast(msg) {
    const node = $("toast");
    node.textContent = msg;
    node.hidden = false;
    clearTimeout(this.toastTimer);
    this.toastTimer = setTimeout(() => { node.hidden = true; }, 1600);
  }

  async deleteNs(q, confirm, page) {
    if (!await this.ask(confirm)) return;
    try {
      await this.api.call("/api/namespaces?repo=" + encodeURIComponent(this.treeRepo) + "&" + q, { method: "DELETE" });
      await this.openTree(this.treeRepo, this.treePrefix, page);
    } catch (e) {
      this.fail(e);
    }
  }

  delNs(ns) {
    return this.deleteNs("ns=" + encodeURIComponent(ns), this.tx("delNs", { ns }), this.treePage);
  }

  delTag(ns, tag) {
    return this.deleteNs("ns=" + encodeURIComponent(ns) + "&tag=" + encodeURIComponent(tag), this.tx("delTag", { ns, tag }), this.treePage);
  }

  async delSelected() {
    const nss = Array.from(document.querySelectorAll(".ns-chk:checked")).map((box) => box.value);
    if (!nss.length) return;
    if (!await this.ask(this.tx("delSel", { n: String(nss.length) }))) return;
    try {
      for (const ns of nss) {
        await this.api.call("/api/namespaces?repo=" + encodeURIComponent(this.treeRepo) + "&ns=" + encodeURIComponent(ns), { method: "DELETE" });
      }
      await this.openTree(this.treeRepo, this.treePrefix, 1);
    } catch (e) {
      this.fail(e);
    }
  }

  delLevel() {
    const prefix = this.treePrefix;
    const label = prefix ? this.treeRepo + "/" + prefix : this.treeRepo;
    return this.deleteNs("prefix=" + encodeURIComponent(prefix), this.tx("delLevel", { label }), 1);
  }

  openDrawer(repo) {
    $("drawer").hidden = false;
    $("drawer-title").textContent = repo ? this.tx("editRepo", { name: repo.name }) : this.tx("newRepo");
    $("r-name").value = repo ? repo.name : "";
    $("r-name").readOnly = Boolean(repo);
    $("r-type").value = repo ? repo.type : "docker";
    $("r-aliases").value = repo ? (repo.aliases || []).join(",") : "";
    $("r-ups").value = repo ? repo.upstreams.map((u) => u.url + " " + (u.proxy || "default")).join("\n") : "";
    $("drawer-msg").textContent = "";
    $("repo-form").dataset.orig = repo ? repo.name : "";
  }

  async saveRepo() {
    const orig = $("repo-form").dataset.orig;
    const name = $("r-name").value.trim();
    const next = {
      name,
      type: $("r-type").value,
      aliases: $("r-aliases").value.split(",").map((s) => s.trim()).filter(Boolean),
      upstreams: $("r-ups").value.split("\n").map((line) => {
        const parts = line.trim().split(/\s+/);
        if (!parts[0]) return null;
        return { url: parts[0], proxy: parts[1] || "default" };
      }).filter(Boolean),
    };
    const copy = structuredClone(this.cfg);
    const idx = copy.repos.findIndex((r) => r.name === (orig || name));
    if (idx >= 0) copy.repos[idx] = Object.assign({}, copy.repos[idx], next);
    else copy.repos.push(next);
    try {
      await this.saveCfg(copy);
      $("drawer").hidden = true;
    } catch (e) {
      $("drawer-msg").textContent = e.message;
    }
  }

  async removeRepo(repo) {
    if (!await this.ask(this.tx("delRepo", { name: repo.name }))) return;
    const copy = structuredClone(this.cfg);
    copy.repos = copy.repos.filter((r) => r.name !== repo.name);
    try {
      await this.saveCfg(copy);
    } catch (e) {
      this.fail(e);
    }
  }

  async saveCfg(next) {
    await this.api.call("/api/config", { method: "PUT", body: JSON.stringify(next) });
    await this.refresh();
    if (this.tabId === "repos") this.showRepoHome();
  }

  renderProbe(map) {
    const root = $("probe-out");
    clear(root);
    const names = Object.keys(map || {});
    if (!names.length) {
      root.append(el("p", { className: "muted", text: this.tx("emptyProbe") }));
      return;
    }
    for (const name of names) {
      const rows = map[name] || [];
      const max = Math.max(1, ...rows.map((r) => r.rtt_ms || 1));
      const card = el("div", { className: "probe-card" }, el("h3", { text: name }));
      for (const row of rows) {
        const pct = row.ok ? Math.max(8, Math.round((1 - (row.rtt_ms || max) / (max * 1.15)) * 100)) : 0;
        const rtt = row.ok ? row.rtt_ms + "ms" : this.tx("failed");
        const bar = el("div", { className: "bar" }, el("i"));
        bar.firstChild.style.width = pct + "%";
        card.append(el("div", { className: "up-row" },
          el("div", {}, el("div", { className: "up-url", text: row.url }), bar),
          el("div", { className: row.ok ? "ok" : "bad", text: rtt }),
        ));
      }
      root.append(card);
    }
  }

  async runProbe() {
    $("probe-msg").textContent = this.tx("probing");
    $("btn-probe").disabled = true;
    try {
      const ranks = await this.api.call("/api/probe", { method: "POST" });
      this.ranks = ranks;
      $("probe-msg").textContent = "";
      this.renderProbe(ranks);
      await this.refresh();
    } catch (e) {
      $("probe-msg").textContent = e.message;
    } finally {
      $("btn-probe").disabled = false;
    }
  }

  prettyCfg() {
    return JSON.stringify(this.cfg, null, 2);
  }

  fillSettings() {
    const box = $("cfg-json");
    const pretty = this.prettyCfg();
    const dirty = box.value !== "" && box.value !== this.lastCfgJson;
    if (!dirty) box.value = pretty;
    this.lastCfgJson = pretty;
    $("cache-hint").textContent = this.cfg.cache ? this.tx("cacheHintOn") : this.tx("cacheHintOff");
    $("lang-select").value = langPref();
  }

  discardSettings() {
    $("cfg-json").value = this.prettyCfg();
    $("save-msg").textContent = "";
  }

  async saveSettings() {
    const raw = $("cfg-json").value;
    let next;
    try {
      next = JSON.parse(raw);
    } catch {
      $("save-msg").textContent = this.tx("jsonInvalid");
      return;
    }
    if (!next || typeof next !== "object" || Array.isArray(next)) {
      $("save-msg").textContent = this.tx("jsonNotObject");
      return;
    }
    try {
      await this.saveCfg(next);
      $("cfg-json").value = this.prettyCfg();
      $("save-msg").textContent = this.tx("saved");
    } catch (e) {
      $("save-msg").textContent = e.message;
    }
  }
}

applyTheme(themeMode());
new App().boot();
