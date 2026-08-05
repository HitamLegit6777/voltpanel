/* ============================================================
   VoltPanel SPA client v3 — SVG icons, i18n, full views
   ============================================================ */
"use strict";

const API = "/api";
const state = { user: null, page: "boot", server: null, servers: [], blueprints: [], pollers: [], consoleEs: null, lang: "en", charts: {}, filePath: "/", fileId: null };

/* ---------- i18n ---------- */
const I18N = {
  en: {
    dashboard: "Fleet", servers: "Workspaces", profile: "Profile", settings: "Settings",
    admin: "Control Center", logout: "Logout", loading: "Loading…", none: "None",
    node: "Fabric", ram: "RAM", disk: "Disk", cpu: "CPU", load: "Load", uptime: "Uptime", processes: "Processes",
    status: "Status", name: "Name", owner: "Owner", actions: "Actions", size: "Size", type: "Type", modified: "Modified",
    start: "Start", restart: "Restart", stop: "Stop", kill: "Kill", save: "Save", cancel: "Cancel", create: "Create",
    delete: "Delete", edit: "Edit", download: "Download", upload: "Upload", copy: "Copy", rename: "Rename",
    console: "Terminal", files: "Storage", databases: "Data Lab", backups: "Vault", schedules: "Flows",
    users: "Team", blueprints: "Blueprint Studio", system: "Observatory", create_server: "New Workspace", create_user: "New Member",
    suspend: "Suspend", unsuspend: "Unsuspend", reinstall: "Rebuild",
    login: "Sign in", username: "Username", password: "Password", remember: "Remember me",
    welcome: "Linux-native workload control plane", no_servers: "No workspaces yet",
    all_servers: "Workspace Fleet", notifications: "Signals", api_keys: "Access Tokens",
    confirm_delete: "Delete?", confirm_restore: "Restore snapshot? Workspace will be stopped.",
    saved: "Saved", created: "Created", deleted: "Deleted", uploaded: "Uploaded",
    twofa: "Two-Factor Auth", enable_2fa: "Enable 2FA", verify: "Verify",
  },
  id: {
    dashboard: "Fleet", servers: "Workspace", profile: "Profil", settings: "Pengaturan",
    admin: "Pusat Kontrol", logout: "Keluar", loading: "Memuat…", none: "Tidak ada",
    node: "Fabric", ram: "RAM", disk: "Disk", cpu: "CPU", load: "Beban", uptime: "Aktif", processes: "Proses",
    status: "Status", name: "Nama", owner: "Pemilik", actions: "Aksi", size: "Ukuran", type: "Tipe", modified: "Diubah",
    start: "Mulai", restart: "Ulang", stop: "Hentikan", kill: "Paksa", save: "Simpan", cancel: "Batal", create: "Buat",
    delete: "Hapus", edit: "Ubah", download: "Unduh", upload: "Unggah", copy: "Salin", rename: "Ganti nama",
    console: "Terminal", files: "Penyimpanan", databases: "Lab Data", backups: "Vault", schedules: "Flow",
    users: "Tim", blueprints: "Studio Blueprint", system: "Observatorium", create_server: "Workspace Baru", create_user: "Anggota Baru",
    suspend: "Nonaktifkan", unsuspend: "Aktifkan", reinstall: "Bangun Ulang",
    login: "Masuk", username: "Nama pengguna", password: "Kata sandi", remember: "Ingat saya",
    welcome: "Control plane workload Linux-native", no_servers: "Belum ada workspace",
    all_servers: "Fleet Workspace", notifications: "Sinyal", api_keys: "Token Akses",
    confirm_delete: "Hapus?", confirm_restore: "Pulihkan snapshot? Workspace akan dihentikan.",
    saved: "Tersimpan", created: "Dibuat", deleted: "Dihapus", uploaded: "Terunggah",
    twofa: "Autentikasi Dua Faktor", enable_2fa: "Aktifkan 2FA", verify: "Verifikasi",
  },
};
const t = (k) => (I18N[state.lang] || I18N.en)[k] || I18N.en[k] || k;

/* ---------- helpers ---------- */
const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const fmtBytes = (b) => { b = +b || 0; if (b >= 1073741824) return (b / 1073741824).toFixed(2) + " GB"; if (b >= 1048576) return (b / 1048576).toFixed(2) + " MB"; if (b >= 1024) return (b / 1024).toFixed(2) + " KB"; return b + " B"; };
const fmtTime = (s) => { s = +s || 0; const d = Math.floor(s / 86400), h = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60); if (d) return `${d}d ${h}h`; if (h) return `${h}h ${m}m`; return `${m}m`; };
const fmtDate = (iso) => iso ? new Date(iso).toLocaleString() : "—";

async function api(path, opts = {}) {
  opts.headers = Object.assign({ "Content-Type": "application/json" }, opts.headers || {});
  const res = await fetch(API + path, opts);
  const ct = res.headers.get("content-type") || "";
  if (res.status === 401 && !path.includes("login")) { renderLogin(); throw new Error("unauthorized"); }
  const body = ct.includes("application/json") ? await res.json() : await res.text();
  if (!res.ok) throw new Error(body.error || body || res.statusText);
  return body;
}

function toast(msg, kind = "info") {
  const wrap = $("#toast-wrap") || document.body;
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.innerHTML = `${ic(kind === "success" ? "check" : kind === "error" ? "xcircle" : kind === "warn" ? "alert" : "info", 16)}<span>${esc(msg)}</span>`;
  wrap.appendChild(el);
  setTimeout(() => { el.style.opacity = "0"; el.style.transition = "opacity .3s"; setTimeout(() => el.remove(), 300); }, 3600);
}

function vpDialog({ title, message = "", input = null, confirmText = "Confirm", danger = false, copyValue = null }) {
  return new Promise((resolve) => {
    const modal = document.createElement("div"); modal.className = "modal dialog-layer";
    modal.innerHTML = `<div class="modal-card dialog-card"><div class="modal-head"><b>${ic(danger ? "alert" : "info",16)}${esc(title)}</b><button class="icon-btn dialog-cancel">${ic("x",16)}</button></div><div class="dialog-body">${message ? `<p>${esc(message)}</p>` : ""}${input !== null ? `<div class="field"><label>Value</label><input class="dialog-input" value="${esc(input)}"></div>` : ""}${copyValue !== null ? `<div class="code-block">${esc(copyValue)}</div>` : ""}</div><div class="modal-foot"><button class="btn ghost dialog-cancel">Cancel</button>${copyValue !== null ? `<button class="btn dialog-copy">${ic("copy",14)}<span>Copy</span></button>` : ""}<button class="btn ${danger ? "danger" : "primary"} dialog-ok">${ic(danger ? "trash" : "check",14)}<span>${esc(confirmText)}</span></button></div></div>`;
    const close = value => { modal.remove(); resolve(value); };
    modal.querySelectorAll(".dialog-cancel").forEach(el => el.addEventListener("click", () => close(false)));
    modal.querySelector(".dialog-copy")?.addEventListener("click", async () => { await navigator.clipboard?.writeText(copyValue); toast("Copied", "success"); });
    modal.querySelector(".dialog-ok").addEventListener("click", () => close(input !== null ? modal.querySelector(".dialog-input").value : true));
    modal.addEventListener("click", e => { if (e.target === modal) close(false); }); document.body.appendChild(modal); modal.querySelector(".dialog-input")?.focus();
  });
}
const vpConfirm = (message, title = "Please confirm") => vpDialog({ title, message, danger: true });
const vpPrompt = (title, value = "") => vpDialog({ title, input: value, confirmText: "Continue" });

/* ---------- router + command palette ---------- */
window.addEventListener("keydown", e => { if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") { e.preventDefault(); openCommandPalette(); } });

function openCommandPalette(){
  const actions=[{name:"Pulse",href:"#/",icon:"activity"},{name:"Workspaces",href:"#/admin/servers",icon:"server"},{name:"Flows",href:"#/automations",icon:"clock"},{name:"Fabric",href:"#/admin/nodes",icon:"globe"},{name:"Blueprint Studio",href:"#/admin/blueprints",icon:"box"},{name:"Observatory",href:"#/admin/system",icon:"gauge"},{name:"Settings",href:"#/settings",icon:"settings"},{name:"Profile",href:"#/profile",icon:"profile"}].filter(a=>state.user?.root_admin||!["Workspaces","Fabric","Blueprint Studio","Observatory"].includes(a.name));
  const modal=document.createElement("div");modal.className="modal palette-layer";modal.innerHTML=`<div class="command-palette"><div class="palette-search">${ic("search",17)}<input placeholder="Search commands and pages..." autocomplete="off"><kbd>ESC</kbd></div><div class="palette-results">${actions.map((a,i)=>`<a href="${a.href}" data-key="${a.name.toLowerCase()}">${ic(a.icon,16)}<span>${a.name}</span><kbd>${i+1}</kbd></a>`).join("")}</div></div>`;
  const input=modal.querySelector("input"),results=[...modal.querySelectorAll(".palette-results a")];const filter=()=>results.forEach(a=>a.hidden=!a.dataset.key.includes(input.value.toLowerCase()));input.addEventListener("input",filter);input.addEventListener("keydown",e=>{if(e.key==="Escape")modal.remove();if(e.key==="Enter"){results.find(a=>!a.hidden)?.click();modal.remove();}});results.forEach(a=>a.addEventListener("click",()=>modal.remove()));modal.addEventListener("click",e=>{if(e.target===modal)modal.remove();});document.body.appendChild(modal);input.focus();
}

function route() {
  killPollers();
  const path = location.hash.slice(1) || "/";
  const parts = path.split("/").filter(Boolean);
  if (parts[0] === "server" && parts[1]) return renderServerPage(parts[1].replace(/\D/g, ""), parts[2] || "console");
  if (parts[0] === "admin") return renderAdmin(parts[1] || "servers");
  if (parts[0] === "profile") return renderProfile();
  if (parts[0] === "automations") return renderAutomations();
  if (parts[0] === "settings") return renderSettings();
  return renderDashboard();
}

window.addEventListener("hashchange", () => route());
window.addEventListener("load", init);

const modalObserver = new MutationObserver(() => {
  const modals=[...document.querySelectorAll(".modal")]; document.body.classList.toggle("modal-open",modals.length>0);
  modals.forEach(modal=>{const dialog=modal.querySelector(".modal-card,.command-palette");if(dialog&&!dialog.hasAttribute("role")){dialog.setAttribute("role","dialog");dialog.setAttribute("aria-modal","true");queueMicrotask(()=>dialog.querySelector("input,button,select,textarea,a[href]")?.focus());}});
});
modalObserver.observe(document.body,{childList:true,subtree:true});
document.addEventListener("keydown",e=>{const modal=[...document.querySelectorAll(".modal")].at(-1);if(!modal)return;if(e.key==="Escape"){modal.remove();return}if(e.key==="Tab"){const focus=[...modal.querySelectorAll("button:not([disabled]),input:not([disabled]),select:not([disabled]),textarea:not([disabled]),a[href]")];if(!focus.length)return;const first=focus[0],last=focus.at(-1);if(e.shiftKey&&document.activeElement===first){e.preventDefault();last.focus()}else if(!e.shiftKey&&document.activeElement===last){e.preventDefault();first.focus()}}});
async function init() {
  try {
    const me = await api("/me");
    state.user = me;
    state.lang = me.language || "en";
    document.documentElement.dataset.theme = me.theme || "dark";
    document.documentElement.lang = state.lang;
    route();
  } catch (e) {
    renderLogin();
  }
}

function killPollers() { state.pollers.forEach((p) => clearInterval(p)); state.pollers = []; if (state.consoleEs) { state.consoleEs.close(); state.consoleEs = null; } }
function poll(fn, ms) { fn(); state.pollers.push(setInterval(fn, ms)); }

/* ---------- layout ---------- */
function shell(active, title, inner) {
  const u = state.user;
  const initial = (u.username || "?").slice(0, 1).toUpperCase();
  return `<div class="layout">
    <div class="sidebar-backdrop" onclick="toggleSidebar()"></div>
    <aside class="sidebar" id="sidebar">
      <div class="brand">
        <div class="brand-mark">${ic("server", 22, 2)}</div>
        <span class="brand-name">Volt<span class="brand-accent">Panel</span></span>
      </div>
      <div class="nav-group-label">CONTROL</div>
      <nav>
        <a href="#/" class="nav-item ${active === "pulse" ? "active" : ""}"><span class="nav-ico">${ic("activity")}</span><span>Pulse</span></a>
        <a href="${u.root_admin?'#/admin/servers':'#/'}" class="nav-item ${active === "workspaces" ? "active" : ""}"><span class="nav-ico">${ic("server")}</span><span>Workspaces</span></a>
        <a href="#/automations" class="nav-item ${active === "flows" ? "active" : ""}"><span class="nav-ico">${ic("clock")}</span><span>Flows</span></a>
      </nav>
      ${u.root_admin ? `<div class="nav-group-label">PLATFORM</div><nav><a href="#/admin/nodes" class="nav-item ${active === "fabric" ? "active" : ""}"><span class="nav-ico">${ic("globe")}</span><span>Fabric</span></a><a href="#/admin/blueprints" class="nav-item ${active === "blueprints" ? "active" : ""}"><span class="nav-ico">${ic("box")}</span><span>Blueprint Studio</span></a><a href="#/admin/system" class="nav-item ${active === "observatory" ? "active" : ""}"><span class="nav-ico">${ic("gauge")}</span><span>Observatory</span></a></nav>` : ""}
      <div class="nav-group-label">ACCOUNT</div><nav><a href="#/settings" class="nav-item ${active === "settings" ? "active" : ""}"><span class="nav-ico">${ic("settings")}</span><span>${t("settings")}</span></a><a href="#/profile" class="nav-item ${active === "profile" ? "active" : ""}"><span class="nav-ico">${ic("profile")}</span><span>${t("profile")}</span></a></nav>
      <div class="side-foot">
        <div class="avatar">${esc(initial)}</div>
        <div class="side-user"><div class="name">${esc(u.username)}</div><div class="role">${u.root_admin ? "Administrator" : "Member"}</div></div>
        <button class="icon-btn" title="${t("logout")}" onclick="logout()">${ic("logout", 17)}</button>
      </div>
    </aside>
    <main class="main">
      <header class="topbar">
        <div class="row"><button class="burger icon-btn" aria-label="Open navigation" onclick="toggleSidebar()">${ic("menu", 20)}</button><h1>${esc(title)}</h1></div>
        <div class="topbar-right">
          <button class="palette-trigger" aria-label="Open command palette" onclick="openCommandPalette()">${ic("search",15)}<span>Search</span><kbd>Ctrl K</kbd></button>
          <button class="icon-btn" aria-label="Toggle color theme" onclick="toggleTheme()">${document.documentElement.dataset.theme === "dark" ? ic("sun", 17) : ic("moon", 17)}</button>
        </div>
        <div id="toast-wrap" role="status" aria-live="polite"></div>
      </header>
      <div class="content">${inner}</div>
    </main>
  </div>`;
}

function toggleSidebar() { $("#sidebar")?.classList.toggle("open"); $(".sidebar-backdrop")?.classList.toggle("show"); }
function toggleTheme() { const el = document.documentElement; el.dataset.theme = el.dataset.theme === "dark" ? "light" : "dark"; api("/profile", { method: "POST", body: JSON.stringify({ theme: el.dataset.theme }) }).catch(() => {}); if (state.user) state.user.theme = el.dataset.theme; }

async function logout() { try { await api("/logout", { method: "POST" }); } catch (e) {} location.hash = ""; renderLogin(); }

/* ============================================================
   AUTH
   ============================================================ */
function renderLogin(err) {
  killPollers();
  state.page = "login";
  document.documentElement.dataset.theme = "dark";
  document.getElementById("app").innerHTML = `<div class="auth-wrap">
    <div class="auth-side">
      <div class="auth-side-inner">
        <div class="auth-logo">${ic("server", 30, 2)}</div>
        <h1 class="auth-title">Volt<span class="brand-accent">Panel</span></h1>
        <p class="auth-sub">${t("welcome")}</p>
        <div class="auth-feats">
          <div class="auth-feat"><span class="feat-ico">${ic("zap", 16)}</span> Rust-native · no Docker</div>
          <div class="auth-feat"><span class="feat-ico">${ic("gauge", 16)}</span> Real-time resource limits</div>
          <div class="auth-feat"><span class="feat-ico">${ic("shield", 16)}</span> argon2id + 2FA</div>
        </div>
      </div>
    </div>
    <div class="auth-main">
      <form class="auth-card" onsubmit="doLogin(event)">
        <h2 class="auth-card-title">${t("login")}</h2>
        ${err ? `<div class="toast error" style="margin-bottom:14px">${ic("xcircle", 16)}<span>${esc(err)}</span></div>` : ""}
        <div class="field">
          <label>${t("username")}</label>
          <div class="field-input">${ic("user", 16)}<input id="l-user" autocomplete="username" required autofocus></div>
        </div>
        <div class="field">
          <label>${t("password")}</label>
          <div class="field-input">${ic("lock", 16)}<input id="l-pass" type="password" autocomplete="current-password" required></div>
        </div>
        <label class="check-row"><input type="checkbox" id="l-rem" checked><span class="check-box">${ic("check", 13, 2.4)}</span><span>${t("remember")}</span></label>
        <button class="btn primary block" type="submit"><span>${t("login")}</span>${ic("chevron_right", 16)}</button>
      </form>
      <div class="auth-foot">VoltPanel v0.1 · Rust · No Docker</div>
    </div>
  </div>`;
  $("#l-user").focus();
}

async function doLogin(e) {
  e.preventDefault();
  try {
    const res = await api("/login", {
      method: "POST",
      body: JSON.stringify({ username: $("#l-user").value, password: $("#l-pass").value, remember: $("#l-rem").checked }),
    });
    if (res.needs_2fa) { render2fa(); return; }
    state.user = res.user;
    state.lang = res.user.language || "en";
    document.documentElement.dataset.theme = res.user.theme || "dark";
    location.hash = "#/";
  } catch (err) { renderLogin(err.message); }
}

function render2fa() {
  document.getElementById("app").innerHTML = `<div class="auth-wrap"><div class="auth-main">
    <form class="auth-card" onsubmit="do2fa(event)">
      <h2 class="auth-card-title">${t("twofa")}</h2>
      <div class="field">
        <label>6-digit code</label>
        <div class="field-input">${ic("lock", 16)}<input id="l-totp" inputmode="numeric" maxlength="6" required autofocus></div>
      </div>
      <button class="btn primary block" type="submit"><span>${t("verify")}</span>${ic("chevron_right", 16)}</button>
    </form>
  </div></div>`;
}

async function do2fa(e) {
  e.preventDefault();
  try {
    const res = await api("/login", {
      method: "POST",
      body: JSON.stringify({ username: state.pendingUser, password: state.pendingPass, totp_code: $("#l-totp").value }),
    });
    state.user = res.user;
    state.lang = res.user.language || "en";
    location.hash = "#/";
  } catch (err) { toast(err.message, "error"); }
}

/* ============================================================
   DASHBOARD
   ============================================================ */
async function renderDashboard() {
  document.getElementById("app").innerHTML = shell("pulse", "Pulse", `
    <section class="fleet-hero">
      <div class="fleet-copy"><span class="eyebrow">LIVE WORKLOAD PULSE</span><h2>See the whole platform breathe.</h2><p>One operational signal across isolated workspaces and every execution agent.</p><div class="fleet-actions"><a href="#/admin/nodes" class="btn primary">${ic("globe",14)}<span>Open fabric</span></a><button class="btn ghost" onclick="refreshServers()">${ic("refresh_ccw",14)}<span>Refresh pulse</span></button></div></div>
      <div class="fleet-visual"><div class="orbit orbit-a"></div><div class="orbit orbit-b"></div><div class="fleet-core">${ic("server",28)}</div><span class="fleet-node n1"></span><span class="fleet-node n2"></span><span class="fleet-node n3"></span></div>
    </section>
    <div class="grid cols-4" id="d-stats">
      <div class="card stat-card"><span class="stat-ico accent">${ic("server", 20)}</span><div class="stat-label">${t("servers")}</div><div class="stat-value" id="d-count">…</div><div class="stat-sub">active instances</div></div>
      <div class="card stat-card"><span class="stat-ico green">${ic("zap", 20)}</span><div class="stat-label">${t("cpu")}</div><div class="stat-value" id="d-cpu">…</div><div class="stat-sub" id="d-load"></div></div>
      <div class="card stat-card"><span class="stat-ico purple">${ic("memory", 20)}</span><div class="stat-label">${t("ram")}</div><div class="stat-value" id="d-mem">…</div><div class="stat-bar"><div id="d-mem-bar" style="width:0%"></div></div></div>
      <div class="card stat-card"><span class="stat-ico yellow">${ic("harddisk", 20)}</span><div class="stat-label">${t("disk")}</div><div class="stat-value" id="d-disk">…</div><div class="stat-bar"><div id="d-disk-bar" style="width:0%"></div></div></div>
    </div>
    <div class="card">
      <div class="card-head"><h3>${t("all_servers")}</h3><button class="icon-btn" title="refresh" onclick="refreshServers()">${ic("refresh_ccw", 16)}</button></div>
      <div id="d-servers"><div class="empty">${ic("box", 40)}<p>${t("loading")}</p></div></div>
    </div>
    <div class="dashboard-lower">
      <section class="card quick-panel"><div class="card-head"><h3>${ic("zap",15)} Quick actions</h3></div><div class="quick-grid">
        ${state.user.root_admin ? `<a href="#/admin/servers" class="quick-action">${ic("plus",18)}<span><b>Compose workspace</b><small>Launch from a VoltSpec blueprint</small></span></a><a href="#/admin/nodes" class="quick-action">${ic("globe",18)}<span><b>Attach agent</b><small>Extend the execution fabric</small></span></a>` : ""}
        <a href="#/settings" class="quick-action">${ic("key",18)}<span><b>API access</b><small>Manage scoped credentials</small></span></a><a href="#/profile" class="quick-action">${ic("shield",18)}<span><b>Account security</b><small>Password and two-factor auth</small></span></a>
      </div></section>
      <section class="card activity-panel"><div class="card-head"><h3>${ic("activity",15)} Recent activity</h3></div><div id="d-activity" class="activity-list"><div class="skeleton" style="height:46px"></div><div class="skeleton" style="height:46px"></div></div></section>
    </div>`);
  const load = async () => {
    try {
      const [stats, list] = await Promise.all([api("/system/stats"), api("/servers")]);
      const servers = list.data || [];
      $("#d-count").textContent = servers.length;
      $("#d-cpu").textContent = Math.round(stats.cpu.usage_percent) + "%";
      $("#d-load").textContent = `load ${stats.load["1"].toFixed(2)}`;
      $("#d-mem").textContent = Math.round(stats.memory.percent) + "%";
      $("#d-mem-bar").style.width = Math.min(100, stats.memory.percent) + "%";
      $("#d-disk").textContent = Math.round(stats.disk.percent) + "%";
      $("#d-disk-bar").style.width = Math.min(100, stats.disk.percent) + "%";
      renderServerTable(servers);
      if (state.user.root_admin) api("/audit").then(r => renderActivity((r.data||[]).slice(0,6))).catch(() => renderActivity([])); else renderActivity([]);
    } catch (e) { toast(e.message, "error"); }
  };
  poll(load, 5000);
}

function renderServerTable(servers) {
  const box = $("#d-servers");
  if (!box) return;
  if (!servers.length) {
    box.innerHTML = `<div class="empty">${ic("box", 40)}<p>${t("no_servers")}</p></div>`;
    return;
  }
  box.innerHTML = `<div class="tbl-wrap"><table class="tbl">
    <thead><tr><th>${t("name")}</th><th>${t("status")}</th><th>${t("cpu")}</th><th>${t("ram")}</th><th>Disk</th><th>${t("uptime")}</th><th></th></tr></thead>
    <tbody>${servers.map((s) => `<tr>
      <td><a href="#/server/${s.id}" class="link-strong">${esc(s.name)}</a><div class="tbl-sub">${esc(s.blueprint)}</div></td>
      <td><span class="pill ${esc(s.status)}"><i></i>${esc(s.status)}</span></td>

      <td>${s.info ? Math.round(s.info.cpu_percent) + "%" : "—"}</td>
      <td>${s.info ? fmtBytes(s.info.memory_bytes) + " / " + fmtBytes(s.memory_mb * 1048576) : "—"}</td>
      <td>${s.info ? fmtBytes(s.info.disk_usage_bytes) + " / " + fmtBytes(s.disk_mb * 1048576) : "—"}</td>
      <td>${s.info ? fmtTime(s.info.uptime_secs) : "—"}</td>
      <td><div class="actions"><a class="btn sm ghost" href="#/server/${s.id}/console">${ic("play", 14)}<span>${t("console")}</span></a></div></td>
    </tr>`).join("")}</tbody></table></div>`;
}

function renderActivity(items) {
  const box = $("#d-activity"); if (!box) return;
  if (!items.length) { box.innerHTML = `<div class="context-empty">${ic("activity",24)}<div><b>No recent control-plane events</b><span>Power, security and provisioning actions will appear here.</span></div></div>`; return; }
  box.innerHTML = items.map(item => `<div class="activity-item"><span class="activity-icon">${ic(item.action?.includes("delete")?"trash":item.action?.includes("login")?"user":item.action?.includes("server")?"server":"activity",14)}</span><div><b>${esc(item.action||"event")}</b><span>${esc(item.target||"system")} · ${fmtDate(item.created_at)}</span></div></div>`).join("");
}

async function refreshServers() { try { const l = await api("/servers"); renderServerTable(l.data || []); } catch (e) { toast(e.message, "error"); } }

/* ---------- automations ---------- */
async function renderAutomations(){
  document.getElementById("app").innerHTML=shell("flows","Flows",`<section class="nodes-header"><div><span class="eyebrow">EVENT-DRIVEN CONTROL</span><h2>Flow runway</h2><p>Coordinate lifecycle commands, snapshots, and tasks across every workspace.</p></div></section><div id="automation-grid" class="node-grid"><div class="skeleton" style="height:180px"></div><div class="skeleton" style="height:180px"></div></div>`);
  try{const servers=(await api("/servers")).data||[];const groups=await Promise.all(servers.map(async server=>({server,schedules:(await api(`/servers/${server.id}/schedules`)).data||[]})));const all=groups.flatMap(g=>g.schedules.map(schedule=>({schedule,server:g.server})));const box=$("#automation-grid");box.innerHTML=all.length?all.map(({schedule,server})=>`<article class="node-card"><div class="node-card-head"><div class="node-mark">${ic("clock",18)}</div><div><h3>${esc(schedule.name)}</h3><span>${esc(server.name)}</span></div><span class="pill ${schedule.enabled?'running':'offline'}"><i></i>${schedule.enabled?'active':'paused'}</span></div><div class="node-endpoint">${ic("terminal",13)}<code>${esc(schedule.cron_expr)}</code></div><div class="metric-line"><span>Next execution</span><b>${fmtDate(schedule.next_run_at)}</b></div><div class="node-card-foot"><span>${schedule.tasks?.length||0} steps</span><a class="btn sm ghost" href="#/server/${server.id}/schedules">Open flow</a></div></article>`).join(""):`<div class="context-empty">${ic("clock",28)}<div><b>No flows configured</b><span>Create one inside a workspace to orchestrate lifecycle, commands, and snapshots.</span></div></div>`;}catch(e){toast(e.message,"error")}
}

/* ============================================================
   SERVER WORKSPACE
   ============================================================ */
async function renderServerPage(id, tab) {
  document.getElementById("app").innerHTML = shell("workspaces", "Workspace", `<div class="empty">${ic("server", 40)}<p>${t("loading")}</p></div>`);
  let data;
  try { data = await api(`/servers/${id}`); } catch (e) { document.querySelector(".content").innerHTML = `<div class="toast error">${ic("xcircle", 16)}<span>${esc(e.message)}</span></div>`; return; }
  const s = data.server;
  const tabs = [
    ["console", ic("terminal", 15) + t("console")], ["files", ic("folder", 15) + t("files")],
    ["settings", ic("settings", 15) + t("settings")], ["databases", ic("database", 15) + t("databases")],
    ["backups", ic("archive", 15) + t("backups")], ["schedules", ic("clock", 15) + t("schedules")],
  ];
  state.page = "server";
  const nav = tabs.map(([tt, label]) => `<a href="#/server/${id}/${tt}" class="${tab === tt ? "active" : ""}">${label}</a>`).join("");
  document.querySelector(".content").innerHTML = `
    <div class="server-head">
      <div>
        <div class="row"><h2 class="server-name">${esc(s.name)}</h2><span class="pill ${esc(s.status)}"><i></i>${esc(s.status)}</span></div>
        <div class="server-meta"><span>${esc(s.blueprint)}</span><span>agent ${esc(s.node)}</span><span>#${s.id}</span>${s.port ? `<button class="allocation-chip" onclick="navigator.clipboard.writeText(location.hostname+':${s.port}');toast('Endpoint copied','success')">${ic("link",12)}${esc(location.hostname)}:${s.port}${ic("copy",11)}</button>` : `<span class="allocation-chip muted">No endpoint</span>`}</div>
      </div>
      <div class="server-actions">
        <button class="btn success" aria-label="Start server" onclick="power('${id}','start')">${ic("play", 14)}<span>${t("start")}</span></button>
        <button class="btn" aria-label="Restart server" onclick="power('${id}','restart')">${ic("refresh", 14)}<span>${t("restart")}</span></button>
        <button class="btn" aria-label="Stop server" onclick="power('${id}','stop')">${ic("stop", 14)}<span>${t("stop")}</span></button>
        <button class="btn danger" aria-label="Force kill server" onclick="confirmKill('${id}')">${ic("x", 14)}<span>${t("kill")}</span></button>
      </div>
    </div>
    <div class="isolation-strip"><div class="isolation-icon">${ic("shield",18)}</div><div><b>Isolated workload</b><span>Private mount, PID, IPC and UTS namespaces · dedicated UID · cgroup v2 limits</span></div><span class="isolation-node">${esc(s.node)}</span></div>
    <div class="server-stats">
      <div class="stat"><span class="st-label">${t("status")}</span><span class="st-val" id="st-status">${esc(s.status)}</span></div>
      <div class="stat"><span class="st-label">${t("cpu")}</span><span class="st-val" id="st-cpu">0%</span></div>
      <div class="stat"><span class="st-label">${t("ram")}</span><span class="st-val" id="st-ram">0</span></div>
      <div class="stat"><span class="st-label">Disk</span><span class="st-val" id="st-disk">0</span></div>
      <div class="stat"><span class="st-label">${t("uptime")}</span><span class="st-val" id="st-up">—</span></div>
      <div class="stat"><span class="st-label">PID</span><span class="st-val" id="st-pid">—</span></div>
    </div>
    <div id="metric-chart" class="metric-chart-grid"></div>
    <div class="tabs">${nav}</div>
    <div id="tab-body"><div class="empty">${ic("server", 40)}<p>${t("loading")}</p></div></div>`;
  state.server = s;
  const render = {
    console: () => renderConsole(id),
    files: () => renderFiles(id),
    settings: () => renderServerSettings(id, data),
    databases: () => renderDatabases(id),
    backups: () => renderBackups(id),
    schedules: () => renderSchedules(id),
  };
  (render[tab] || render.console)();
  poll(async () => {
    try {
      const st = await api(`/servers/${id}/stats`);
      $("#st-status").textContent = st.status;
      $("#st-status").className = `st-val ${st.status === "running" ? "green" : st.status === "crashed" ? "red" : ""}`;
      $("#st-cpu").textContent = Math.round(st.cpu) + "%";
      $("#st-ram").textContent = fmtBytes(st.memory_bytes) + " / " + st.memory_limit_mb + " MB";
      $("#st-disk").textContent = fmtBytes(st.disk_bytes) + " / " + st.disk_limit_mb + " MB";
      $("#st-up").textContent = fmtTime(st.uptime_secs);
      $("#st-pid").textContent = st.pid || "—";
      pushMetric(id, st);
    } catch (e) {}
  }, 3000);
}

async function power(id, action) {
  try { await api(`/servers/${id}/power`, { method: "POST", body: JSON.stringify({ action }) }); toast(`${t(action || "ok")} → ok`, "success"); }
  catch (e) { toast(e.message, "error"); }
}

/* ---------- console ---------- */
function renderConsole(id) {
  $("#tab-body").innerHTML = `<div class="console-wrap">
    <div class="console-bar"><span>${ic("terminal", 14)} console — ${esc(state.server.name)}</span><span class="console-dots"><i></i><i></i><i></i></span></div>
    <div class="console" id="console-out"></div>
    <div class="console-input">${ic("chevron_right", 16)}<input id="console-cmd" placeholder="Type a command…" autocomplete="off"><button class="btn" onclick="sendCmd('${id}')">${ic("send", 14)}<span>Send</span></button></div>
  </div>`;

  const out = $("#console-out");
  const es = new EventSource(`/api/servers/${id}/console/stream`);
  es.addEventListener("console", (ev) => {
    const pre = document.createElement("span");
    pre.textContent = ev.data;
    out.appendChild(pre);
    out.scrollTop = out.scrollHeight;
    if (out.childNodes.length > 2000) out.innerHTML = "";
  });
  es.onerror = () => {};
  $("#console-cmd").addEventListener("keydown", (e) => { if (e.key === "Enter") sendCmd(id); });
  state.consoleEs = es;
}

async function confirmKill(id) {
  if (await vpConfirm("Force-kill this process group? Unsaved data may be lost.", "Force kill server")) power(id, "kill");
}

async function sendCmd(id) {
  const inp = $("#console-cmd");
  if (!inp.value.trim()) return;
  try { await api(`/servers/${id}/console/command`, { method: "POST", body: JSON.stringify({ command: inp.value }) }); }
  catch (e) { toast(e.message, "error"); }
  inp.value = "";
}

/* ---------- files ---------- */
async function renderFiles(id) {
  state.fileId = id;
  $("#tab-body").innerHTML = `<div class="files-toolbar">
    <div class="crumbs" id="f-crumbs"></div>
    <div class="spacer"></div>
    <button class="icon-btn" title="${t("upload")}" onclick="fileUpload('${id}')">${ic("upload", 16)}</button>
    <button class="icon-btn" title="new file" onclick="fileNewFile('${id}')">${ic("file", 16)}</button>
    <button class="icon-btn" title="new folder" onclick="fileNewDir('${id}')">${ic("folder", 16)}</button>
    <button class="icon-btn" title="refresh" onclick="loadFiles('${id}', state.filePath)">${ic("refresh_ccw", 16)}</button>
  </div>
  <div id="file-list"><div class="empty">${ic("folder", 40)}<p>${t("loading")}</p></div></div>
  <input type="file" id="file-picker" multiple style="display:none">`;
  await loadFiles(id, "/");
}

async function loadFiles(id, path) {
  state.filePath = path;
  const parts = path.split("/").filter(Boolean);
  let crumb = `<a href="#" onclick="event.preventDefault();loadFiles('${id}','/')">/</a>`;
  let acc = "";
  parts.forEach((p, i) => {
    acc += "/" + p;
    crumb += `<span class="crumb-sep">/</span><a href="#" onclick="event.preventDefault();loadFiles('${id}','${esc(acc)}')">${esc(p)}</a>`;
  });
  $("#f-crumbs").innerHTML = crumb;
  try {
    const res = await api(`/servers/${id}/files?path=${encodeURIComponent(path)}`);
    const entries = res.data || [];
    if (!entries.length) { $("#file-list").innerHTML = `<div class="file-list"><div class="empty">${ic("folder", 40)}<p>${t("none")}</p></div></div>`; return; }
    $("#file-list").innerHTML = `<div class="file-list">
      <div class="file-row head"><span></span><span>${t("name")}</span><span>${t("size")}</span><span>${t("type")}</span><span>${t("actions")}</span></div>
      ${entries.map((f) => `<div class="file-row">
        <span class="f-icon">${fileIcon(f.extension, f.is_dir)}</span>
        <span class="f-name" ${f.is_dir ? `onclick="loadFiles('${id}','${esc(f.path)}')"` : `onclick="fileOpen('${id}','${esc(f.path)}')"`}>${esc(f.name)}</span>
        <span class="f-meta">${f.is_dir ? "—" : fmtBytes(f.size)}</span>
        <span class="f-meta">${esc((f.mime || "file").split("/")[0])}</span>
        <span class="f-actions">
          <button class="icon-btn sm" title="${t("download")}" onclick="fileDl('${id}','${esc(f.path)}')">${ic("download", 15)}</button>
          <button class="icon-btn sm" title="${t("rename")}" onclick="fileRename('${id}','${esc(f.path)}')">${ic("pencil", 15)}</button>
          <button class="icon-btn sm" title="${t("copy")}" onclick="fileCopy('${id}','${esc(f.path)}')">${ic("copy", 15)}</button>
          <button class="icon-btn sm danger" title="${t("delete")}" onclick="fileDel('${id}','${esc(f.path)}')">${ic("trash", 15)}</button>
        </span>
      </div>`).join("")}
    </div>`;
  } catch (e) { toast(e.message, "error"); }
}

async function fileOpen(id, path) {
  try {
    const res = await api(`/servers/${id}/files/read?path=${encodeURIComponent(path)}`);
    const content = res.content_b64 ? atob(res.content_b64) : "";
    const modal = document.createElement("div");
    modal.className = "modal";
    modal.innerHTML = `<div class="modal-card big">
      <div class="modal-head"><b>${fileIcon(res.mime.includes("json") ? "json" : "txt", false)} <span class="modal-title">${esc(path)}</span></b><div class="row"><span class="badge">${esc(res.mime)}</span><button class="icon-btn" onclick="this.closest('.modal').remove()">${ic("x", 16)}</button></div></div>
      <textarea id="editor" spellcheck="false">${esc(content)}</textarea>
      <div class="modal-foot"><button class="btn ghost" onclick="this.closest('.modal').remove()">${t("cancel")}</button><button class="btn primary" onclick="fileSave('${id}','${esc(path)}')">${ic("save", 14)}<span>${t("save")}</span></button></div>
    </div>`;
    document.body.appendChild(modal);
    $("#editor").focus();
  } catch (e) { toast(e.message, "error"); }
}

async function fileSave(id, path) {
  const content = $("#editor").value;
  try { await api(`/servers/${id}/files/write`, { method: "POST", body: JSON.stringify({ path, content }) }); toast(t("saved"), "success"); $(".modal")?.remove(); }
  catch (e) { toast(e.message, "error"); }
}
async function fileDl(id, path) { window.location = `/api/servers/${id}/files/download?path=${encodeURIComponent(path)}`; }
async function fileDel(id, path) { if (!await vpConfirm(`${t("confirm_delete")} ${path}`)) return; try { await api(`/servers/${id}/files/delete`, { method: "POST", body: JSON.stringify({ path }) }); toast(t("deleted"), "success"); await loadFiles(id, state.filePath); } catch (e) { toast(e.message, "error"); } }
async function fileRename(id, path) { const name = await vpPrompt("New name:", path.split("/").pop()); if (!name) return; const to = path.slice(0, path.lastIndexOf("/")) + "/" + name; try { await api(`/servers/${id}/files/rename`, { method: "POST", body: JSON.stringify({ from: path, to }) }); await loadFiles(id, state.filePath); } catch (e) { toast(e.message, "error"); } }
async function fileCopy(id, path) { const to = path + ".copy"; try { await api(`/servers/${id}/files/copy`, { method: "POST", body: JSON.stringify({ from: path, to }) }); await loadFiles(id, state.filePath); } catch (e) { toast(e.message, "error"); } }
async function fileNewFile(id) { const name = await vpPrompt("File name:"); if (!name) return; try { await api(`/servers/${id}/files/touch`, { method: "POST", body: JSON.stringify({ path: state.filePath + "/" + name }) }); await loadFiles(id, state.filePath); } catch (e) { toast(e.message, "error"); } }
async function fileNewDir(id) { const name = await vpPrompt("Directory name:"); if (!name) return; try { await api(`/servers/${id}/files/mkdir`, { method: "POST", body: JSON.stringify({ path: state.filePath + "/" + name }) }); await loadFiles(id, state.filePath); } catch (e) { toast(e.message, "error"); } }
function fileUpload(id) { const picker = $("#file-picker"); picker.onchange = async () => { const fd = new FormData(); for (const f of picker.files) fd.append("file", f, f.name); try { await fetch(`/api/servers/${id}/files/upload`, { method: "POST", body: fd }); toast(t("uploaded"), "success"); await loadFiles(id, state.filePath); } catch (e) { toast(e.message, "error"); } }; picker.click(); }

/* ---------- server settings ---------- */
function renderServerSettings(id, data) {
  const s = data.server;
  const vars = (data.variables || []).map((v) => `<div class="field">
    <label>${esc(v.name)} <code>${esc(v.env_var)}</code> ${v.user_editable ? "" : '<span class="badge">locked</span>'}</label>
    <div class="field-input">${ic("pencil", 14)}<input data-var="${esc(v.env_var)}" value="${esc(v.value)}" ${v.user_editable ? "" : "disabled"}></div>
    <small>${esc(v.description)}</small>
  </div>`).join("");
  const subUsers = (data.subusers || []).map((su) => `<div class="file-row"><span class="avatar mini">${esc(su.username[0] || "?").toUpperCase()}</span><b>${esc(su.username)}</b><span class="f-meta">${(su.permissions || []).join(", ") || "all"}</span><span class="f-actions"><button class="icon-btn sm danger" onclick="subDel('${id}',${su.id})">${ic("trash", 15)}</button></span></div>`).join("");
  $("#tab-body").innerHTML = `
    <div class="grid cols-2">
      <div class="card"><h3>${ic("terminal", 15)} Launch Plan</h3><div class="code-block">${esc(data.resolved_launch)}</div>
        <div class="field" style="margin-top:12px"><label>Runtime hint</label><div class="field-input">${ic("box", 14)}<input id="s-runtime" value="${esc(data.runtime_hint)}" ${state.user.root_admin ? "" : "disabled"}></div></div>
        <div class="row"><button class="btn sm" onclick="saveRuntime('${id}')">${ic("save", 13)}<span>${t("save")}</span></button></div>
      </div>
      <div class="card"><h3>${ic("settings", 15)} ${t("settings")}</h3>
        <div class="metric-line"><span>Auto-restart on crash</span><label class="switch"><input type="checkbox" id="s-autorestart" ${s.auto_restart ? "checked" : ""} onchange="saveToggle('${id}','auto_restart',this.checked)"><i></i></label></div>
        <div class="metric-line"><span>Memory</span><b>${s.memory_mb} MB</b></div>
        <div class="metric-line"><span>Disk</span><b>${s.disk_mb} MB</b></div>
        <div class="metric-line"><span>CPU limit</span><b>${s.cpu_percent}%</b></div>
        <div class="metric-line"><span>Restart count</span><b>${s.restart_count}</b></div>
        <div class="metric-line"><span>Execution agent</span><b>${esc(s.node)}</b></div>
      </div>
    </div>
    <div class="card"><h3>${ic("sliders", 15)} Variables</h3>${vars || `<div class="muted">${t("none")}</div>`}<button class="btn primary" onclick="saveVars('${id}')">${ic("save", 14)}<span>${t("save")} Variables</span></button></div>
    <div class="grid cols-2">
      <div class="card"><h3>${ic("users", 15)} Subusers</h3>${subUsers || `<div class="muted">${t("none")}</div>`}
        <div class="row" style="margin-top:10px"><input id="sub-user-id" placeholder="user id" style="width:90px"><button class="btn sm" onclick="subAdd('${id}')">${ic("plus", 13)}<span>${t("create")}</span></button></div>
      </div>
      <div class="card"><h3>${ic("alert", 15)} Danger Zone</h3>
        <div class="row" style="flex-wrap:wrap;gap:8px">
          <button class="btn" onclick="install('${id}')">${ic("refresh", 13)}<span>${t("reinstall")}</span></button>
          <button class="btn" onclick="suspend('${id}')">${ic("pause", 13)}<span>${t("suspend")}</span></button>
          <button class="btn danger" onclick="delServer('${id}')">${ic("trash", 13)}<span>${t("delete")}</span></button>
        </div>
      </div>
    </div>`;
}

async function saveRuntime(id) { const runtime_hint = $("#s-runtime").value; try { await api(`/servers/${id}`, { method: "PATCH", body: JSON.stringify({ runtime_hint }) }); toast(t("saved"), "success"); } catch (e) { toast(e.message, "error"); } }
async function saveToggle(id, field, val) { try { await api(`/servers/${id}`, { method: "PATCH", body: JSON.stringify({ [field]: val }) }); toast(t("saved"), "success"); } catch (e) { toast(e.message, "error"); } }
async function saveVars(id) {
  const variables = {};
  $$("[data-var]").forEach((inp) => { variables[inp.dataset.var] = inp.value; });
  try { await api(`/servers/${id}/variables`, { method: "POST", body: JSON.stringify({ variables }) }); toast(t("saved"), "success"); }
  catch (e) { toast(e.message, "error"); }
}
async function subAdd(id) { const user_id = +$("#sub-user-id").value; if (!user_id) return; try { await api(`/servers/${id}/subusers`, { method: "POST", body: JSON.stringify({ user_id, permissions: ["start", "stop", "console", "files"] }) }); toast(t("created"), "success"); location.reload(); } catch (e) { toast(e.message, "error"); } }
async function subDel(id, sub_id) { if (!await vpConfirm(t("confirm_delete"))) return; try { await fetch(`/api/servers/${id}/subusers/${sub_id}`, { method: "DELETE" }); location.reload(); } catch (e) { toast(e.message, "error"); } }
async function install(id) { try { await api(`/servers/${id}/install`, { method: "POST", body: JSON.stringify({}) }); toast("Install queued", "success"); } catch (e) { toast(e.message, "error"); } }
async function suspend(id) { try { await api(`/servers/${id}/suspend`, { method: "POST" }); toast(t("saved"), "success"); } catch (e) { toast(e.message, "error"); } }
async function delServer(id) { if (!await vpConfirm(`${t("confirm_delete")} ${id}?`)) return; try { await fetch(`/api/servers/${id}`, { method: "DELETE" }); location.hash = "#/"; } catch (e) { toast(e.message, "error"); } }

/* ---------- databases ---------- */
async function renderDatabases(id) {
  $("#tab-body").innerHTML = `<div class="card"><h3>${ic("database", 15)} ${t("databases")} <span class="badge">SQLite</span></h3>
    <div id="db-list"><div class="empty">${ic("database", 40)}<p>${t("loading")}</p></div></div>
    <div class="field-row" style="margin-top:12px"><input id="db-name" placeholder="db name"><button class="btn primary" onclick="dbCreate('${id}')">${ic("plus", 14)}<span>${t("create")}</span></button></div>
  </div>`;
  await dbLoad(id);
}
async function dbLoad(id) {
  try {
    const res = await api(`/servers/${id}/databases`);
    const dbs = res.data || [];
    $("#db-list").innerHTML = dbs.length ? `<div class="file-list">${dbs.map((d) => `<div class="file-row"><span class="f-icon">${ic("database", 16)}</span><b>${esc(d.name)}</b><span class="f-meta">${fmtBytes(d.size)}</span><span class="f-actions"><button class="icon-btn sm" onclick="dbOpen('${id}','${esc(d.name)}')">${ic("terminal", 15)}</button><button class="icon-btn sm danger" onclick="dbDrop('${id}','${esc(d.name)}')">${ic("trash", 15)}</button></span></div>`).join("")}</div>` : `<div class="empty">${ic("database", 40)}<p>${t("none")}</p></div>`;
  } catch (e) { toast(e.message, "error"); }
}
async function dbCreate(id) { const name = $("#db-name").value.trim(); if (!name) return; try { await api(`/servers/${id}/databases`, { method: "POST", body: JSON.stringify({ name }) }); toast(t("created"), "success"); await dbLoad(id); } catch (e) { toast(e.message, "error"); } }
async function dbDrop(id, name) { if (!await vpConfirm(`${t("confirm_delete")} ${name}?`)) return; try { await fetch(`/api/servers/${id}/databases/${name}`, { method: "DELETE" }); await dbLoad(id); } catch (e) { toast(e.message, "error"); } }
async function dbOpen(id, name) {
  const modal = document.createElement("div");
  modal.className = "modal";
  modal.innerHTML = `<div class="modal-card big">
    <div class="modal-head"><b>${ic("database", 16)} <span class="modal-title">${esc(name)}</span></b><button class="icon-btn" onclick="this.closest('.modal').remove()">${ic("x", 16)}</button></div>
    <textarea id="db-sql" placeholder="SELECT * FROM sqlite_master;" spellcheck="false">SELECT * FROM sqlite_master WHERE type='table';</textarea>
    <div class="modal-foot"><button class="btn primary" onclick="dbExec('${id}','${esc(name)}')">${ic("play", 13)}<span>Run</span></button></div>
    <pre id="db-out" class="db-out"></pre>
  </div>`;
  document.body.appendChild(modal);
  dbExec(id, name);
}
async function dbExec(id, name) {
  const sql = $("#db-sql").value;
  try { const res = await api(`/servers/${id}/databases/${name}/query`, { method: "POST", body: JSON.stringify({ sql }) }); $("#db-out").textContent = JSON.stringify(res, null, 2); }
  catch (e) { $("#db-out").textContent = "Error: " + e.message; }
}

/* ---------- backups ---------- */
async function renderBackups(id) {
  $("#tab-body").innerHTML = `<div class="card"><h3>${ic("archive", 15)} ${t("backups")} <span class="badge">zip + sha256</span></h3>
    <div class="row" style="margin-bottom:12px"><button class="btn primary" onclick="bkCreate('${id}')">${ic("plus", 14)}<span>${t("create")}</span></button><span class="muted" style="font-size:12px">auto-cleanup keeps newest</span></div>
    <div id="bk-list"><div class="empty">${ic("archive", 40)}<p>${t("loading")}</p></div></div></div>`;
  await bkLoad(id);
}
async function bkLoad(id) {
  try {
    const res = await api(`/servers/${id}/backups`);
    const bks = res.data || [];
    $("#bk-list").innerHTML = bks.length ? `<div class="file-list">${bks.map((b) => `<div class="file-row"><span class="f-icon">${ic("archive", 16)}</span><b>${esc(b.name)}</b><span class="f-meta">${fmtBytes(b.size_bytes)}</span><span class="f-meta">${fmtDate(b.created_at)}</span><span class="f-actions">
      <a class="icon-btn sm" title="${t("download")}" href="/api/backups/${b.id}/download">${ic("download", 15)}</a>
      <button class="icon-btn sm" title="restore" onclick="bkRestore('${id}',${b.id})">${ic("refresh_ccw", 15)}</button>
      <button class="icon-btn sm danger" title="${t("delete")}" onclick="bkDel(${b.id})">${ic("trash", 15)}</button>
    </span></div>`).join("")}</div>` : `<div class="empty">${ic("archive", 40)}<p>${t("none")}</p></div>`;
  } catch (e) { toast(e.message, "error"); }
}
async function bkCreate(id) { try { await api(`/servers/${id}/backups`, { method: "POST", body: JSON.stringify({}) }); toast(t("created"), "success"); await bkLoad(id); } catch (e) { toast(e.message, "error"); } }
async function bkRestore(id, bid) { if (!await vpConfirm(t("confirm_restore"))) return; try { await api(`/backups/${bid}/restore`, { method: "POST" }); toast("Restored", "success"); } catch (e) { toast(e.message, "error"); } }
async function bkDel(bid) { if (!await vpConfirm(t("confirm_delete"))) return; try { await fetch(`/api/backups/${bid}/delete`, { method: "DELETE" }); toast(t("deleted"), "success"); } catch (e) { toast(e.message, "error"); } }

/* ---------- schedules ---------- */
async function renderSchedules(id) {
  $("#tab-body").innerHTML = `<div class="card"><h3>${ic("clock", 15)} ${t("schedules")} <span class="badge">cron</span></h3>
    <div id="sch-list"><div class="empty">${ic("clock", 40)}<p>${t("loading")}</p></div></div>
    <div class="field-row" style="margin-top:12px"><input id="sch-name" placeholder="name"><input id="sch-cron" placeholder="sec min hour day mon dow" value="0 0 4 * * *"><button class="btn primary" onclick="schCreate('${id}')">${ic("plus", 14)}<span>${t("create")}</span></button></div>
    <small class="muted">Format: <code>sec min hour day month weekday</code> — daily 04:00 restart: <code>0 0 4 * * *</code></small>
  </div>`;
  await schLoad(id);
}
async function schLoad(id) {
  try {
    const res = await api(`/servers/${id}/schedules`);
    const schs = res.data || [];
    $("#sch-list").innerHTML = schs.length ? `<div class="file-list">${schs.map((s) => `<div class="file-row"><span class="f-icon">${ic("clock", 16)}</span><b>${esc(s.name)}</b><code>${esc(s.cron_expr)}</code><span class="pill ${s.enabled ? "running" : "offline"}"><i></i>${s.enabled ? "on" : "off"}</span><span class="f-meta">next: ${s.next_run_at ? fmtDate(s.next_run_at) : "—"}</span><span class="f-actions">
      <button class="icon-btn sm" onclick="schToggle(${s.id},${s.enabled ? "false" : "true"})">${s.enabled ? ic("pause", 14) : ic("play", 14)}</button>
      <button class="icon-btn sm" title="run now" onclick="schRun(${s.id})">${ic("zap", 14)}</button>
      <button class="icon-btn sm danger" onclick="schDel(${s.id})">${ic("trash", 14)}</button>
    </span></div>`).join("")}</div>` : `<div class="empty">${ic("clock", 40)}<p>${t("none")}</p></div>`;
  } catch (e) { toast(e.message, "error"); }
}
async function schCreate(id) { const name = $("#sch-name").value; const cron = $("#sch-cron").value; if (!name || !cron) return; try { await api(`/servers/${id}/schedules`, { method: "POST", body: JSON.stringify({ name, cron_expr: cron, enabled: true, tasks: [{ action: "restart", payload: "", sequence: 1 }] }) }); toast(t("created"), "success"); await schLoad(id); } catch (e) { toast(e.message, "error"); } }
async function schToggle(id, on) { try { await api(`/schedules/${id}/toggle/${on}`, { method: "POST" }); await schLoad(state.server?.id); } catch (e) { toast(e.message, "error"); } }
async function schRun(id) { try { await api(`/schedules/${id}/run`, { method: "POST" }); toast("Triggered", "success"); } catch (e) { toast(e.message, "error"); } }
async function schDel(id) { if (!await vpConfirm(t("confirm_delete"))) return; try { await fetch(`/api/schedules/${id}`, { method: "DELETE" }); await schLoad(state.server?.id); } catch (e) { toast(e.message, "error"); } }

/* ---------- metric sparkline ---------- */
function pushMetric(id, st) {
  if (state.page !== "server") return;
  state.charts[id] = state.charts[id] || { cpu: [], mem: [], ts: [] };
  const c = state.charts[id];
  c.cpu.push(st.cpu); c.mem.push(st.memory_percent); c.ts.push(Date.now());
  if (c.cpu.length > 60) { c.cpu.shift(); c.mem.shift(); c.ts.shift(); }
  const holder = $("#metric-chart");
  if (!holder) return;
  holder.innerHTML = `<div class="card"><h3>${ic("activity", 15)} CPU (60s)</h3><div class="sparkline">${sparkSvg(c.cpu)}</div>
    <h3 style="margin-top:14px">${ic("memory", 15)} Memory (60s)</h3><div class="sparkline">${sparkSvg(c.mem, "var(--purple)")}</div></div>`;
}
function sparkSvg(data, color = "var(--accent)") {
  if (!data.length) return "<div class='muted'>—</div>";
  const w = 600, h = 40;
  const max = Math.max(...data, 1);
  const pts = data.map((v, i) => `${(i / (data.length - 1)) * w},${h - (v / max) * (h - 4) - 2}`).join(" ");
  return `<svg viewBox="0 0 ${w} ${h}" preserveAspectRatio="none"><polygon class="fill" points="0,${h} ${pts} ${w},${h}"/><polyline points="${pts}" style="stroke:${color};fill:none;stroke-width:1.6"/></svg>`;
}

/* ============================================================
   PROFILE
   ============================================================ */
function renderProfile() {
  const u = state.user;
  document.getElementById("app").innerHTML = shell("profile", t("profile"), `
    <div class="grid cols-2">
      <div class="card"><h3>${ic("profile", 15)} Account</h3>
        <div class="row" style="margin-bottom:16px"><div class="avatar lg">${esc(u.username[0] || "?").toUpperCase()}</div>
          <div><b style="font-size:16px">${esc(u.username)}</b><div class="muted">${u.root_admin ? "Administrator" : "Member"} · joined ${fmtDate(u.created_at)}</div></div></div>
        <div class="field"><label>Email</label><div class="field-input">${ic("send", 14)}<input id="p-email" value="${esc(u.email)}"></div></div>
        <div class="field"><label>About</label><textarea id="p-about" rows="2">${esc(u.about || "")}</textarea></div>
        <div class="field"><label>Language</label><select id="p-lang"><option value="en" ${u.language === "en" ? "selected" : ""}>English</option><option value="id" ${u.language === "id" ? "selected" : ""}>Bahasa Indonesia</option></select></div>
        <div class="field"><label>Theme</label><select id="p-theme"><option value="dark" ${u.theme === "dark" ? "selected" : ""}>Dark</option><option value="light" ${u.theme === "light" ? "selected" : ""}>Light</option></select></div>
        <button class="btn primary" onclick="saveProfile()">${ic("save", 14)}<span>${t("save")}</span></button>
      </div>
      <div>
        <div class="card"><h3>${ic("lock", 15)} ${t("password")}</h3>
          <div class="field"><label>Current</label><div class="field-input">${ic("lock", 14)}<input type="password" id="p-cur" autocomplete="current-password"></div></div>
          <div class="field"><label>New</label><div class="field-input">${ic("key", 14)}<input type="password" id="p-new" autocomplete="new-password"></div></div>
          <button class="btn primary" onclick="savePass()">Change</button>
        </div>
        <div class="card" style="margin-top:16px"><h3>${ic("shield", 15)} ${t("twofa")}</h3>
          <p class="muted" style="margin-bottom:10px">${u.twofa_secret ? "2FA is enabled" : "2FA is disabled"}</p>
          <div class="row"><button class="btn" onclick="setup2fa()">${ic("shield", 14)}<span>${t("enable_2fa")}</span></button></div>
        </div>
      </div>
    </div>`);
}
async function saveProfile() {
  try {
    const u = await api("/profile", { method: "POST", body: JSON.stringify({ email: $("#p-email").value, about: $("#p-about").value, language: $("#p-lang").value, theme: $("#p-theme").value }) });
    state.user = u; state.lang = u.language || "en";
    document.documentElement.dataset.theme = u.theme;
    document.documentElement.lang = state.lang;
    toast(t("saved"), "success");
  } catch (e) { toast(e.message, "error"); }
}
async function savePass() {
  try { await api("/password", { method: "POST", body: JSON.stringify({ current: $("#p-cur").value, new: $("#p-new").value }) }); toast("Password changed", "success"); $("#p-cur").value = ""; $("#p-new").value = ""; }
  catch (e) { toast(e.message, "error"); }
}
async function setup2fa() {
  try {
    const res = await api("/2fa/setup");
    const modal = document.createElement("div");
    modal.className = "modal";
    modal.innerHTML = `<div class="modal-card">
      <div class="modal-head"><b>${ic("shield", 15)} ${t("enable_2fa")}</b><button class="icon-btn" onclick="this.closest('.modal').remove()">${ic("x", 16)}</button></div>
      <div style="text-align:center;padding:16px 20px">
        <img class="totp-qr" src="data:image/png;base64,${res.qr_b64}" alt="Authenticator QR code">
        <p class="muted" style="margin:10px 0">Scan with Google Authenticator / Authy</p>
        <p class="muted">Secret: <code>${esc(res.secret)}</code></p>
      </div>
      <div class="field" style="padding:0 20px"><label>6-digit code</label><input id="2fa-code" inputmode="numeric" maxlength="6"></div>
      <div class="modal-foot"><button class="btn primary" onclick="confirm2fa('${esc(res.secret)}')">${t("verify")} →</button></div>
    </div>`;
    document.body.appendChild(modal);
  } catch (e) { toast(e.message, "error"); }
}
async function confirm2fa(secret) {
  try { await api("/2fa/confirm", { method: "POST", body: JSON.stringify({ secret, code: $("#2fa-code").value }) }); toast("2FA enabled", "success"); $(".modal")?.remove(); renderProfile(); }
  catch (e) { toast(e.message, "error"); }
}

/* ============================================================
   SETTINGS
   ============================================================ */
async function renderSettings() {
  document.getElementById("app").innerHTML = shell("settings", t("settings"), `
    <div class="grid cols-2">
      <div class="card"><h3>${ic("key", 15)} ${t("api_keys")}</h3><div id="keys-list"><div class="empty">${ic("key", 40)}<p>${t("loading")}</p></div></div>
        <div class="field-row" style="margin-top:12px"><input id="key-name" placeholder="key name"><button class="btn primary" onclick="keyCreate()">${ic("plus", 14)}<span>${t("create")}</span></button></div>
      </div>
      <div class="card"><h3>${ic("bell", 15)} ${t("notifications")}</h3><div id="notif-list"><div class="empty">${ic("bell", 40)}<p>${t("loading")}</p></div></div></div>
    </div>`);
  await keyLoad(); await notifLoad();
}
async function keyLoad() {
  try {
    const res = await api("/keys");
    const keys = res.data || [];
    $("#keys-list").innerHTML = keys.length ? `<div class="file-list">${keys.map((k) => `<div class="file-row"><span class="f-icon">${ic("key", 16)}</span><b>${esc(k.name)}</b><span class="f-meta">${esc(k.scopes)}</span><span class="f-meta">${esc(k.last_used || "never")}</span><span class="f-actions"><button class="icon-btn sm danger" onclick="keyDel(${k.id})">${ic("trash", 15)}</button></span></div>`).join("")}</div>` : `<div class="empty">${ic("key", 40)}<p>${t("none")}</p></div>`;
  } catch (e) { toast(e.message, "error"); }
}
async function keyCreate() {
  const name = $("#key-name").value.trim(); if (!name) return;
  try { const res = await api("/keys", { method: "POST", body: JSON.stringify({ name }) });
    const modal = document.createElement("div"); modal.className = "modal";
    modal.innerHTML = `<div class="modal-card"><div class="modal-head"><b>${ic("key", 15)} Token created</b><button class="icon-btn" onclick="this.closest('.modal').remove()">${ic("x", 16)}</button></div>
      <div style="padding:16px 20px"><p class="muted">Show once only — copy now:</p><div class="code-block">${esc(res.token)}</div></div>
      <div class="modal-foot"><button class="btn primary" onclick="this.closest('.modal').remove()">OK</button></div></div>`;
    document.body.appendChild(modal); keyLoad();
  } catch (e) { toast(e.message, "error"); }
}
async function keyDel(id) { if (!await vpConfirm(t("confirm_delete"))) return; try { await fetch(`/api/keys/${id}`, { method: "DELETE" }); keyLoad(); } catch (e) { toast(e.message, "error"); } }
async function notifLoad() {
  try {
    const res = await api("/notifications");
    const notifs = (res.data || []).slice(-30).reverse();
    $("#notif-list").innerHTML = notifs.length ? `<div class="file-list">${notifs.map((n) => `<div class="file-row"><span class="pill ${esc(n.level)}"><i></i>${esc(n.level)}</span><b>${esc(n.title)}</b><span class="f-meta">${fmtDate(n.created_at)}</span></div>`).join("")}</div>` : `<div class="empty">${ic("bell", 40)}<p>${t("none")}</p></div>`;
  } catch (e) {}
}

/* ============================================================
   ADMIN
   ============================================================ */
function renderAdmin(tab) {
  if (!state.user?.root_admin) { toast("Control Center only", "error"); renderDashboard(); return; }
  const active=tab==="nodes"?"fabric":tab==="blueprints"?"blueprints":tab==="system"?"observatory":"workspaces";
  document.getElementById("app").innerHTML = shell(active, "Control Center", `<div class="tabs">
    <a href="#/admin/servers" class="${tab === "servers" ? "active" : ""}">${ic("server", 14)} Workspaces</a>
    <a href="#/admin/users" class="${tab === "users" ? "active" : ""}">${ic("users", 14)} Team</a>
    <a href="#/admin/blueprints" class="${tab === "blueprints" ? "active" : ""}">${ic("box", 14)} Blueprint Studio</a>
    <a href="#/admin/nodes" class="${tab === "nodes" ? "active" : ""}">${ic("globe", 14)} Fabric</a>
    <a href="#/admin/system" class="${tab === "system" ? "active" : ""}">${ic("gauge", 14)} Observatory</a>
  </div><div id="admin-body"><div class="empty">${ic("shield", 40)}<p>${t("loading")}</p></div></div>`);
  const render = { servers: adminServers, users: adminUsers, blueprints: adminBlueprints, nodes: adminNodes, system: adminSystem };
  (render[tab] || adminServers)();
}

async function adminServers() {
  $("#admin-body").innerHTML = `<div class="card"><div class="card-head"><h3>${t("all_servers")}</h3><button class="btn primary sm" onclick="adminNewServer()">${ic("plus", 14)}<span>${t("create_server")}</span></button></div><div id="a-servers"></div></div>`;
  try {
    const res = await api("/servers/all");
    const servers = res.data || [];
    $("#a-servers").innerHTML = `<div class="tbl-wrap"><table class="tbl"><thead><tr><th>ID</th><th>${t("name")}</th><th>${t("owner")}</th><th>${t("status")}</th><th>RAM</th><th>Disk</th><th></th></tr></thead><tbody>` +
      servers.map((s) => `<tr>
        <td>${s.id}</td><td><a href="#/server/${s.id}" class="link-strong">${esc(s.name)}</a></td>
        <td>#${s.user_id}</td><td><span class="pill ${esc(s.status)}"><i></i>${esc(s.status)}</span></td>
        <td>${s.memory_mb} MB</td><td>${s.disk_mb} MB</td>
        <td><div class="actions"><button class="icon-btn sm" onclick="adminToggleSuspend(${s.id},${s.suspended ? "false" : "true"})">${s.suspended ? ic("play", 15) : ic("pause", 15)}</button><button class="icon-btn sm danger" onclick="adminDelServer(${s.id})">${ic("trash", 15)}</button></div></td>
      </tr>`).join("") + `</tbody></table></div>`;
  } catch (e) { toast(e.message, "error"); }
}
async function adminToggleSuspend(id, on) { try { await api(`/servers/${id}/${on ? "unsuspend" : "suspend"}`, { method: "POST" }); adminServers(); } catch (e) { toast(e.message, "error"); } }
async function adminDelServer(id) { if (!await vpConfirm(`${t("confirm_delete")} ${id}?`)) return; try { await fetch(`/api/servers/${id}`, { method: "DELETE" }); adminServers(); toast(t("deleted"), "success"); } catch (e) { toast(e.message, "error"); } }
async function adminNewServer() {
  try {
    const [usersRes, blueprintsRes] = await Promise.all([api("/admin/users"), api("/blueprints")]);
    const users = usersRes.data || usersRes;
    const blueprints = blueprintsRes.data || blueprintsRes;
    const modal = document.createElement("div"); modal.className = "modal";
    modal.innerHTML = `<div class="modal-card">
      <div class="modal-head"><b>${ic("server", 15)} ${t("create_server")}</b><button class="icon-btn" onclick="this.closest('.modal').remove()">${ic("x", 16)}</button></div>
      <div class="field"><label>${t("name")}</label><input id="ns-name" placeholder="my-workspace"></div>
      <div class="field"><label>${t("owner")}</label><select id="ns-user">${users.map((u) => `<option value="${u.id}">${esc(u.username)} (#${u.id})</option>`).join("")}</select></div>
      <div class="field"><label>Blueprint</label><select id="ns-blueprint">${blueprints.map((definition) => `<option value="${definition.id}">${esc(definition.name)} — ${esc(definition.category)}</option>`).join("")}</select></div>
      <div class="grid cols-3">
        <div class="field"><label>RAM (MB)</label><input id="ns-mem" type="number" value="1024"></div>
        <div class="field"><label>Disk (MB)</label><input id="ns-disk" type="number" value="8192"></div>
        <div class="field"><label>CPU %</label><input id="ns-cpu" type="number" value="100"></div>
      </div>
      <div class="modal-foot"><button class="btn ghost" onclick="this.closest('.modal').remove()">${t("cancel")}</button><button class="btn primary" onclick="adminCreateServer()">${ic("plus", 14)}<span>${t("create")}</span></button></div>
    </div>`;
    document.body.appendChild(modal);
  } catch (e) { toast(e.message, "error"); }
}
async function adminCreateServer() {
  try {
    await api("/servers", { method: "POST", body: JSON.stringify({ name: $("#ns-name").value, user_id: +$("#ns-user").value, blueprint_id: +$("#ns-blueprint").value, memory_mb: +$("#ns-mem").value, disk_mb: +$("#ns-disk").value, cpu_percent: +$("#ns-cpu").value, start_on_create: false }) });
    $(".modal")?.remove(); adminServers(); toast(t("created"), "success");
  } catch (e) { toast(e.message, "error"); }
}

async function adminUsers() {
  $("#admin-body").innerHTML = `<div class="card"><div class="card-head"><h3>${t("users")}</h3><button class="btn primary sm" onclick="adminNewUser()">${ic("plus", 14)}<span>${t("create_user")}</span></button></div><div id="a-users"></div></div>`;
  try {
    const res = await api("/admin/users");
    const users = res.data || res;
    $("#a-users").innerHTML = `<div class="tbl-wrap"><table class="tbl"><thead><tr><th>ID</th><th>${t("username")}</th><th>Email</th><th>Admin</th><th>Active</th><th>2FA</th><th></th></tr></thead><tbody>` +
      users.map((u) => `<tr>
        <td>${u.id}</td><td><b>${esc(u.username)}</b></td><td>${esc(u.email)}</td>
        <td>${u.root_admin ? '<span class="pill success plain">'+ic("shield", 12)+'admin</span>' : "—"}</td><td>${u.active ? '<span class="pill running">active</span>' : '<span class="pill error">off</span>'}</td>
        <td>${u.twofa_secret ? ic("shield", 15) : "—"}</td>
        <td><div class="actions"><button class="btn xs ghost" onclick="adminToggleUser(${u.id},'root_admin',${!u.root_admin})">${u.root_admin ? "demote" : "promote"}</button>
        <button class="btn xs ghost" onclick="adminToggleUser(${u.id},'active',${!u.active})">${u.active ? "disable" : "enable"}</button>
        <button class="icon-btn sm danger" onclick="adminDeleteUser(${u.id})">${ic("trash", 15)}</button></div></td>
      </tr>`).join("") + `</tbody></table></div>`;
  } catch (e) { toast(e.message, "error"); }
}
async function adminToggleUser(id, field, val) { try { await fetch(`/api/admin/users/${id}`, { method: "PATCH", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ [field]: val }) }); adminUsers(); } catch (e) { toast(e.message, "error"); } }
async function adminDeleteUser(id) { if (!await vpConfirm(`${t("confirm_delete")} ${id}?`)) return; try { await fetch(`/api/admin/users/${id}`, { method: "DELETE" }); adminUsers(); } catch (e) { toast(e.message, "error"); } }
function adminNewUser() {
  const modal = document.createElement("div"); modal.className = "modal";
  modal.innerHTML = `<div class="modal-card">
    <div class="modal-head"><b>${ic("user", 15)} ${t("create_user")}</b><button class="icon-btn" onclick="this.closest('.modal').remove()">${ic("x", 16)}</button></div>
    <div class="field"><label>${t("username")}</label><input id="nu-user"></div>
    <div class="field"><label>Email</label><input id="nu-email"></div>
    <div class="field"><label>${t("password")}</label><input id="nu-pass" type="password"></div>
    <label class="check-row" style="padding:0 20px;margin-bottom:14px"><input type="checkbox" id="nu-admin"><span class="check-box">${ic("check", 13, 2.4)}</span><span>Administrator</span></label>
    <div class="modal-foot"><button class="btn ghost" onclick="this.closest('.modal').remove()">${t("cancel")}</button><button class="btn primary" onclick="adminCreateUser()">${ic("plus", 14)}<span>${t("create")}</span></button></div>
  </div>`;
  document.body.appendChild(modal);
}
async function adminCreateUser() {
  try { await api("/admin/users", { method: "POST", body: JSON.stringify({ username: $("#nu-user").value, email: $("#nu-email").value, password: $("#nu-pass").value, root_admin: $("#nu-admin").checked }) }); $(".modal")?.remove(); adminUsers(); toast(t("created"), "success"); }
  catch (e) { toast(e.message, "error"); }
}

async function adminBlueprints() {
  $("#admin-body").innerHTML = `<section class="nodes-header"><div><span class="eyebrow">VOLT SPECIFICATION</span><h2>Blueprint Studio</h2><p>Compose portable launch plans for games, sites, bots, and long-running services.</p></div><button class="btn primary sm" onclick="adminNewBlueprint()">${ic("plus",14)}<span>New blueprint</span></button></section><div id="a-blueprints" class="blueprint-grid"></div>`;
  try {
    const res = await api("/blueprints"); const blueprints = res.data || res;
    $("#a-blueprints").innerHTML = blueprints.length ? blueprints.map((definition) => {const count=definition.variables?.length||0;const kind=String(definition.category||"").toLowerCase();const symbol=kind==="database"?"database":kind==="web"?"globe":kind==="game"?"zap":kind==="generic"?"terminal":"blueprint";return `<article class="blueprint-card"><div class="blueprint-card-head"><span class="blueprint-symbol">${ic(symbol,20)}</span><div><h3>${esc(definition.name)}</h3><span>${esc(definition.category)} · VoltSpec</span></div></div><p>${esc(definition.description || "Reusable isolated workload plan")}</p><div class="blueprint-command"><span>Launch</span><code title="${esc(definition.startup || "operator-defined")}">${esc(definition.startup || "operator-defined")}</code></div><div class="blueprint-card-foot"><span>${count} ${count===1?'input':'inputs'}</span><div class="actions"><button class="icon-btn sm" title="Export VoltSpec" onclick="adminBlueprintExport(${definition.id})">${ic("download",15)}</button><button class="icon-btn sm danger" title="Delete blueprint" onclick="adminDeleteBlueprint(${definition.id})">${ic("trash",15)}</button></div></div></article>`}).join("") : `<div class="context-empty">${ic("blueprint",28)}<div><b>No blueprints yet</b><span>Create the first portable VoltSpec launch plan.</span></div></div>`;
  } catch (e) { toast(e.message,"error"); }
}
async function adminBlueprintExport(id){try{const r=await api(`/blueprints/${id}/export`);await navigator.clipboard?.writeText(r.json);toast("VoltSpec copied","success")}catch(e){toast(e.message,"error")}}
async function adminDeleteBlueprint(id){if(!await vpConfirm(`Delete blueprint ${id}?`))return;try{await api(`/blueprints/${id}`,{method:"DELETE"});adminBlueprints()}catch(e){toast(e.message,"error")}}
function adminNewBlueprint(){const modal=document.createElement("div");modal.className="modal";modal.innerHTML=`<div class="modal-card"><div class="modal-head"><b>${ic("box",15)} New VoltSpec blueprint</b><button class="icon-btn" onclick="this.closest('.modal').remove()">${ic("x",16)}</button></div><div class="field"><label>Name</label><input id="nb-name" placeholder="Velocity Proxy"></div><div class="field"><label>Category</label><input id="nb-category" value="application"></div><div class="field"><label>Runtime hint</label><input id="nb-runtime" value="linux-native"></div><div class="field"><label>Launch plan</label><input id="nb-launch" placeholder="java -jar app.jar"></div><div class="modal-foot"><button class="btn ghost" onclick="this.closest('.modal').remove()">Cancel</button><button class="btn primary" onclick="adminCreateBlueprint()">${ic("plus",14)}<span>Create blueprint</span></button></div></div>`;document.body.appendChild(modal)}
async function adminCreateBlueprint(){try{await api("/blueprints",{method:"POST",body:JSON.stringify({name:$("#nb-name").value,category:$("#nb-category").value,docker_image:$("#nb-runtime").value,startup:$("#nb-launch").value})});$(".modal")?.remove();adminBlueprints();toast("Blueprint created","success")}catch(e){toast(e.message,"error")}}

async function adminNodes() {
  $("#admin-body").innerHTML = `<section class="nodes-header"><div><span class="eyebrow">EXECUTION FABRIC</span><h2>Agent mesh</h2><p>Capacity, isolation, placement, and health across every execution host.</p></div><button class="btn primary" onclick="adminNewNode()">${ic("plus",14)}<span>Attach agent</span></button></section><div id="a-nodes" class="node-grid"></div>`;
  try {
    const res = await api("/nodes"); const values = res.data || [];
    $("#a-nodes").innerHTML = values.length ? values.map(n => { const cpu=Math.round(n.capacity?.cpu_percent||0), mem=n.capacity?.memory_total?Math.round(n.capacity.memory_used/n.capacity.memory_total*100):0; return `<article class="node-card ${n.online?'online':'offline'}">
      <div class="node-card-head"><div class="node-mark">${ic('server',20)}</div><div><h3>${esc(n.name)}</h3><span>${esc(n.location||'unassigned')}</span></div><span class="pill ${n.online?'running':'offline'}"><i></i>${n.online?'online':'offline'}</span></div>
      <div class="node-endpoint">${ic('link',13)}<code>${esc(n.public_url)}</code></div>
      <div class="node-metrics"><div><span>CPU</span><b>${cpu}%</b><div class="progress"><div style="width:${cpu}%"></div></div></div><div><span>Memory</span><b>${mem}%</b><div class="progress"><div style="width:${mem}%;background:var(--purple)"></div></div></div></div>
      <div class="node-security">${ic('shield',15)}<span>Namespace + cgroup isolation</span><b>${n.online?'verified':'pending'}</b></div>
      <div class="node-card-foot"><span>${n.capacity?.servers_running||0} running / ${n.capacity?.servers_total||0} total</span><div class="actions"><button class="icon-btn" title="test" onclick="nodeTest(${n.id})">${ic('activity',15)}</button><button class="icon-btn" title="re-enroll" onclick="nodeReenroll(${n.id})">${ic('key',15)}</button><button class="icon-btn danger" title="delete" onclick="nodeDelete(${n.id})">${ic('trash',15)}</button></div></div></article>`; }).join('') : `<div class="empty">${ic('globe',40)}<p>No agents attached. Local isolated execution remains available.</p></div>`;
  } catch(e) { toast(e.message,'error'); }
}

function adminNewNode() {
  const modal=document.createElement('div'); modal.className='modal'; modal.innerHTML=`<div class="modal-card"><div class="modal-head"><b>${ic('globe',15)} Attach agent</b><button class="icon-btn" onclick="this.closest('.modal').remove()">${ic('x',16)}</button></div>
    <div class="field"><label>Name</label><input id="nn-name" placeholder="agent-eu-1"></div><div class="field"><label>Agent endpoint</label><input id="nn-url" value="http://127.0.0.1:8081"></div><div class="field"><label>Location</label><input id="nn-location" placeholder="id-jakarta"></div>
    <div class="modal-foot"><button class="btn ghost" onclick="this.closest('.modal').remove()">Cancel</button><button class="btn primary" onclick="nodeCreate()">${ic('plus',14)}<span>Create</span></button></div></div>`; document.body.appendChild(modal);
}
async function nodeCreate(){try{const r=await api('/nodes',{method:'POST',body:JSON.stringify({name:$('#nn-name').value,public_url:$('#nn-url').value,location:$('#nn-location').value,tags:[]})}); $('.modal')?.remove(); const cmd = `./voltd join ${location.origin} ${r.enrollment_token} --public-url ${r.node.public_url}${location.protocol === "http:" ? " --allow-http" : ""}`; const m=document.createElement('div');m.className='modal';m.innerHTML=`<div class="modal-card"><div class="modal-head"><b>One-command setup</b><button class="icon-btn" onclick="this.closest('.modal').remove()">${ic('x',16)}</button></div><div style="padding:18px"><p class="muted">Run this on the node machine:</p><div class="code-block" style="margin-top:10px">${esc(cmd)}</div></div><div class="modal-foot"><button class="btn primary" onclick="navigator.clipboard.writeText('${esc(cmd)}');toast('Copied','success')">Copy command</button></div></div>`;document.body.appendChild(m);adminNodes();}catch(e){toast(e.message,'error')}}
async function nodeTest(id){try{const r=await api(`/nodes/${id}/test`,{method:'POST'});toast(`Agent online · ${r.latency_ms}ms`,'success')}catch(e){toast(e.message,'error')}}
async function nodeReenroll(id){try{const r=await api(`/nodes/${id}/enrollment`,{method:'POST'});await vpPrompt('Enrollment token', r.enrollment_token);adminNodes()}catch(e){toast(e.message,'error')}}
async function nodeDelete(id){if(!await vpConfirm('Detach agent?'))return;try{await fetch(`/api/nodes/${id}`,{method:'DELETE'});adminNodes()}catch(e){toast(e.message,'error')}}

async function adminSystem() {
  $("#admin-body").innerHTML = `<div class="grid cols-4" id="a-node-grid"></div>
    <div class="grid cols-2">
      <div class="card"><h3>${ic("link", 15)} Allocations</h3><div id="a-alloc"><div class="empty">${ic("link", 40)}<p>${t("loading")}</p></div></div></div>
      <div class="card"><h3>${ic("gauge", 15)} Host resources</h3><div id="a-res"></div></div>
    </div>`;
  try {
    const s = await api("/system/stats");
    $("#a-node-grid").innerHTML = `
      <div class="card stat-card"><span class="stat-ico green">${ic("zap", 20)}</span><div class="stat-label">${t("cpu")}</div><div class="stat-value">${Math.round(s.cpu.usage_percent)}%</div><div class="stat-sub">freq ${Math.round(s.cpu.frequency_mhz / 1000)} GHz</div></div>
      <div class="card stat-card"><span class="stat-ico purple">${ic("memory", 20)}</span><div class="stat-label">${t("ram")}</div><div class="stat-value">${Math.round(s.memory.percent)}%</div><div class="stat-sub">${fmtBytes(s.memory.used * 1024)} / ${fmtBytes(s.memory.total * 1024)}</div></div>
      <div class="card stat-card"><span class="stat-ico yellow">${ic("harddisk", 20)}</span><div class="stat-label">${t("disk")}</div><div class="stat-value">${Math.round(s.disk.percent)}%</div><div class="stat-sub">${fmtBytes(s.disk.used)} / ${fmtBytes(s.disk.total)}</div></div>
      <div class="card stat-card"><span class="stat-ico accent">${ic("clock", 20)}</span><div class="stat-label">${t("uptime")}</div><div class="stat-value">${fmtTime(s.uptime_secs)}</div><div class="stat-sub">${s.processes} ${t("processes")}</div></div>`;
    $("#a-res").innerHTML = `
      <div class="metric-line"><span>Load 1m / 5m / 15m</span><b>${s.load["1"].toFixed(2)} / ${s.load["5"].toFixed(2)} / ${s.load["15"].toFixed(2)}</b></div>
      <div class="metric-line"><span>CPU</span><b>${Math.round(s.cpu.usage_percent)}%</b></div><div class="progress"><div style="width:${Math.min(100, s.cpu.usage_percent)}%;background:var(--accent)"></div></div>
      <div class="metric-line"><span>RAM</span><b>${Math.round(s.memory.percent)}%</b></div><div class="progress"><div style="width:${Math.min(100, s.memory.percent)}%;background:var(--purple)"></div></div>
      <div class="metric-line"><span>Disk</span><b>${Math.round(s.disk.percent)}%</b></div><div class="progress"><div style="width:${Math.min(100, s.disk.percent)}%;background:var(--yellow)"></div></div>`;
    const alloc = await api("/system/allocations");
    const allocs = alloc.data || [];
    $("#a-alloc").innerHTML = allocs.length ? `<div class="file-list">${allocs.map((a) => `<div class="file-row"><span class="f-icon">${ic("link", 16)}</span><b>${a.port}</b><span class="f-meta">→ ${esc(a.server || "free")}</span></div>`).join("")}</div>` : `<div class="empty">${ic("link", 40)}<p>${t("none")}</p></div>`;
  } catch (e) { toast(e.message, "error"); }
}
