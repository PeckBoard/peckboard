// The Linux App Manager dashboard: one self-contained HTML page served under
// /plugin-api/* and framed in a SANDBOXED iframe (no same-origin), so it talks
// to the plugin's own authenticated routes through the parent-proxied fetch
// bridge — never directly, never to another plugin.
//
// Everything the page renders is already shaped server-side by src/view.ts
// (badges, action labels, job headlines, error prose), so the script below is
// pure DOM plumbing. Two conventions worth keeping:
//   - the inline script uses ONLY string concatenation (no backticks / ${}),
//     so it nests cleanly inside this template literal;
//   - data is written with textContent, never innerHTML.

export const PAGE = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>App Manager</title>
<script>
  (function () {
    var t = new URLSearchParams(location.search).get("theme");
    if (t === "dark" || t === "light") document.documentElement.dataset.theme = t;
  })();
</script>
<style>
  /* Light is the default; dark applies via an explicit <html data-theme="dark">
     stamp (?theme=dark|light in the iframe URL) or prefers-color-scheme, with
     the stamp winning both ways. Same token set as the SSH Fleet page. */
  :root {
    --bg: #f6f8fa; --panel: #ffffff; --panel2: #eef1f5; --line: #d0d7de;
    --fg: #1f2328; --muted: #57606a; --accent: #0969da; --accent2: #0550ae;
    --ok: #1a7f37; --err: #cf222e; --warn: #9a6700; --idle: #6e7781;
    --sel: #dbe9f8; --badge-bg: #ddf4ff; --badge-line: #99c9ef;
    --ok-bg: #dafbe1; --ok-line: #a0d8b0; --warn-bg: #fff8c5; --warn-line: #e0c96b;
    --err-bg: #ffebe9; --err-line: #f0a5a0;
    --shadow: 0 8px 24px rgba(140,149,159,.3); --overlay: rgba(27,31,36,.5);
    color-scheme: light;
  }
  :root[data-theme="dark"] {
    --bg: #0f1419; --panel: #171d26; --panel2: #1e2631; --line: #2a333f;
    --fg: #e6edf3; --muted: #8b98a5; --accent: #4c9be8; --accent2: #2d7dd2;
    --ok: #3fb950; --err: #f85149; --warn: #d29922; --idle: #6e7681;
    --sel: #1b2b3d; --badge-bg: #12283f; --badge-line: #1d3a58;
    --ok-bg: #10251a; --ok-line: #1f5132; --warn-bg: #2b2312; --warn-line: #5c4813;
    --err-bg: #2d1618; --err-line: #6b2b2b;
    --shadow: 0 8px 24px rgba(0,0,0,.4); --overlay: rgba(0,0,0,.55);
    color-scheme: dark;
  }
  @media (prefers-color-scheme: dark) {
    :root:not([data-theme="light"]) {
      --bg: #0f1419; --panel: #171d26; --panel2: #1e2631; --line: #2a333f;
      --fg: #e6edf3; --muted: #8b98a5; --accent: #4c9be8; --accent2: #2d7dd2;
      --ok: #3fb950; --err: #f85149; --warn: #d29922; --idle: #6e7681;
      --sel: #1b2b3d; --badge-bg: #12283f; --badge-line: #1d3a58;
      --ok-bg: #10251a; --ok-line: #1f5132; --warn-bg: #2b2312; --warn-line: #5c4813;
      --err-bg: #2d1618; --err-line: #6b2b2b;
      --shadow: 0 8px 24px rgba(0,0,0,.4); --overlay: rgba(0,0,0,.55);
      color-scheme: dark;
    }
  }
  * { box-sizing: border-box; }
  html, body { margin: 0; height: 100%; }
  body {
    background: var(--bg); color: var(--fg);
    font: 13px/1.5 system-ui, -apple-system, Segoe UI, Roboto, sans-serif;
    display: flex; flex-direction: column; height: 100vh;
  }
  header {
    display: flex; align-items: center; gap: 10px; padding: 10px 16px;
    border-bottom: 1px solid var(--line); background: var(--panel); flex: 0 0 auto;
  }
  header h1 { font-size: 15px; margin: 0; font-weight: 600; letter-spacing: .2px; }
  header .spacer { flex: 1; }
  .pick { display: flex; align-items: center; gap: 6px; }
  .pick label { color: var(--muted); font-size: 12px; }
  .sub { color: var(--muted); font-size: 12px; }
  button {
    background: var(--panel2); color: var(--fg); border: 1px solid var(--line);
    border-radius: 6px; padding: 6px 10px; cursor: pointer; font-size: 12px;
  }
  button:hover:enabled { border-color: var(--accent); }
  button:focus-visible, select:focus-visible, input:focus-visible, a:focus-visible {
    outline: 2px solid var(--accent); outline-offset: 1px;
  }
  button:disabled { opacity: .5; cursor: not-allowed; }
  button.primary { background: var(--accent2); border-color: var(--accent2); color: #fff; }
  button.primary:hover:enabled { background: var(--accent); }
  button.danger { color: var(--err); }
  button.danger.primary { background: var(--err); border-color: var(--err); color: #fff; }
  input, select, textarea {
    background: var(--bg); color: var(--fg); border: 1px solid var(--line);
    border-radius: 6px; padding: 6px 8px; font: inherit; width: 100%;
  }
  select { min-width: 180px; }
  input:focus, select:focus { outline: none; border-color: var(--accent); }
  .banner {
    padding: 8px 16px; border-bottom: 1px solid var(--line); background: var(--panel2);
    display: flex; align-items: center; gap: 8px; flex: 0 0 auto; font-size: 12px;
  }
  .banner.bad { background: var(--err-bg); border-bottom-color: var(--err-line); }
  .banner.warn { background: var(--warn-bg); border-bottom-color: var(--warn-line); }
  .banner .pm { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
  .layout { flex: 1; display: flex; min-height: 0; }
  main { flex: 1; overflow: auto; min-width: 0; }
  .grid { display: flex; flex-direction: column; }
  .approw {
    display: flex; gap: 12px; align-items: flex-start; padding: 12px 16px;
    border-bottom: 1px solid var(--line);
  }
  .approw:hover { background: var(--panel); }
  .approw.installed { border-left: 3px solid var(--ok); }
  .approw.missing { border-left: 3px solid transparent; }
  .approw .body { flex: 1; min-width: 0; }
  .approw .name { font-weight: 600; display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
  .approw .desc { color: var(--muted); margin-top: 2px; }
  .approw .ver {
    margin-top: 3px; font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 11px; color: var(--muted); word-break: break-all;
  }
  .approw .acts { display: flex; flex-direction: column; align-items: flex-end; gap: 4px; }
  .approw .why { color: var(--muted); font-size: 11px; max-width: 200px; text-align: right; }
  .badge {
    font-size: 10px; border-radius: 10px; padding: 1px 8px; border: 1px solid var(--line);
    background: var(--panel2); color: var(--muted); white-space: nowrap;
  }
  .badge.ok { background: var(--ok-bg); border-color: var(--ok-line); color: var(--ok); }
  .badge.busy { background: var(--badge-bg); border-color: var(--badge-line); color: var(--accent); }
  .badge.bad { background: var(--err-bg); border-color: var(--err-line); color: var(--err); }
  .empty { padding: 32px 16px; color: var(--muted); text-align: center; }
  aside.log {
    width: 420px; flex: 0 0 420px; border-left: 1px solid var(--line); background: var(--panel);
    display: flex; flex-direction: column; min-height: 0;
  }
  aside.log[hidden] { display: none; }
  .loghead {
    display: flex; align-items: center; gap: 8px; padding: 8px 12px;
    border-bottom: 1px solid var(--line);
  }
  .loghead .title { font-weight: 600; flex: 1; min-width: 0; }
  .logbody {
    flex: 1; overflow: auto; margin: 0; padding: 10px 12px; white-space: pre-wrap;
    word-break: break-word; font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 11px; color: var(--fg);
  }
  /* dialogs */
  .backdrop {
    position: fixed; inset: 0; background: var(--overlay); display: none;
    align-items: center; justify-content: center; z-index: 50; padding: 16px;
  }
  .backdrop.open { display: flex; }
  .modal {
    background: var(--panel); border: 1px solid var(--line); border-radius: 10px;
    width: 460px; max-width: 100%; max-height: 90vh; overflow: auto; box-shadow: var(--shadow);
  }
  .modal h2 { margin: 0; padding: 14px 16px; border-bottom: 1px solid var(--line); font-size: 14px; }
  .modal .form { padding: 14px 16px; display: flex; flex-direction: column; gap: 10px; }
  .modal .foot { padding: 12px 16px; border-top: 1px solid var(--line); display: flex; justify-content: flex-end; gap: 8px; }
  .field { display: flex; flex-direction: column; gap: 4px; }
  .field label { font-size: 11px; color: var(--muted); }
  .row2 { display: flex; gap: 10px; }
  .row2 > * { flex: 1; }
  .hint { font-size: 11px; color: var(--muted); }
  .formerr { color: var(--err); font-size: 12px; min-height: 16px; }
  .warnbox {
    background: var(--warn-bg); border: 1px solid var(--warn-line); border-radius: 6px;
    padding: 8px 10px; font-size: 12px;
  }
  .toast {
    position: fixed; bottom: 16px; right: 16px; background: var(--panel2);
    border: 1px solid var(--line); border-radius: 8px; padding: 10px 14px; z-index: 60;
    max-width: 460px; display: none; white-space: pre-wrap; box-shadow: var(--shadow);
  }
  .toast.open { display: block; }
  .toast.bad { border-color: var(--err-line); background: var(--err-bg); }

  @media (max-width: 860px) {
    header { flex-wrap: wrap; }
    .pick { flex: 1 1 100%; }
    .pick select { flex: 1; }
    .layout { flex-direction: column; }
    aside.log { width: auto; flex: 0 0 auto; max-height: 45vh; border-left: none; border-top: 1px solid var(--line); }
    input, select { font-size: 16px; } /* keep iOS from zooming on focus */
    .approw { flex-wrap: wrap; }
    .approw .why { text-align: left; max-width: none; }
  }
</style>
</head>
<body>
<header>
  <h1>App Manager</h1>
  <div class="pick">
    <label for="targetSel">Target</label>
    <select id="targetSel" aria-describedby="targetDetail"></select>
  </div>
  <span class="sub" id="targetDetail"></span>
  <div class="spacer"></div>
  <button id="addTargetBtn">+ Add remote target</button>
  <button id="editTargetBtn">Edit target</button>
  <button id="removeTargetBtn" class="danger">Remove target</button>
  <button id="refreshBtn" class="primary">Refresh</button>
</header>

<div class="banner" id="banner" role="status" aria-live="polite"><span id="bannerText">Loading…</span></div>

<div class="layout">
  <main>
    <div class="grid" id="grid"></div>
    <div class="empty" id="gridEmpty">Loading applications…</div>
  </main>
  <aside class="log" id="logPanel" hidden aria-labelledby="logTitle">
    <div class="loghead">
      <span class="title" id="logTitle"></span>
      <span class="badge" id="logStatus"></span>
      <button id="logClose" aria-label="Close the log panel">✕</button>
    </div>
    <pre class="logbody" id="logTail" tabindex="0" aria-live="polite"></pre>
  </aside>
</div>

<div class="backdrop" id="targetBackdrop">
  <div class="modal" role="dialog" aria-modal="true" aria-labelledby="targetModalTitle">
    <h2 id="targetModalTitle">Add remote target</h2>
    <div class="form">
      <div class="field">
        <label for="f_label">Label</label>
        <input id="f_label" placeholder="build-box" />
      </div>
      <div class="row2">
        <div class="field" style="flex:2">
          <label for="f_hostname">Hostname or IP *</label>
          <input id="f_hostname" placeholder="10.0.0.5" />
        </div>
        <div class="field">
          <label for="f_port">Port</label>
          <input id="f_port" type="number" min="1" max="65535" value="22" />
        </div>
      </div>
      <div class="field">
        <label for="f_username">Username *</label>
        <input id="f_username" placeholder="ubuntu" />
      </div>
      <div class="field">
        <label for="f_key_id">SSH key *</label>
        <select id="f_key_id"></select>
        <span class="hint" id="keyHint">Keys live in Peckboard's vault (Settings → SSH Keys) and are chosen by name. This page never sees or stores private key material.</span>
      </div>
      <div class="field">
        <label for="f_known">Pinned host key fingerprint (optional)</label>
        <input id="f_known" placeholder="SHA256:…" />
      </div>
      <p class="formerr" id="targetErr"></p>
    </div>
    <div class="foot">
      <button id="targetCancel">Cancel</button>
      <button id="targetSave" class="primary">Save target</button>
    </div>
  </div>
</div>

<div class="backdrop" id="confirmBackdrop">
  <div class="modal" role="dialog" aria-modal="true" aria-labelledby="confirmTitle" aria-describedby="confirmBody">
    <h2 id="confirmTitle">Confirm</h2>
    <div class="form">
      <p id="confirmBody" style="margin:0"></p>
      <p class="warnbox" id="confirmWarn"></p>
    </div>
    <div class="foot">
      <button id="confirmCancel">Cancel</button>
      <button id="confirmOk" class="primary danger">Confirm</button>
    </div>
  </div>
</div>

<div class="toast" id="toast" role="status" aria-live="polite"></div>

<script>
(function () {
  "use strict";

  // ── Parent-proxied fetch bridge (sandboxed iframe, no same-origin). ──
  var _pending = {}, _seq = 0;
  window.addEventListener("message", function (e) {
    var m = e.data;
    if (m && m.type === "plugin-ui-fetch-result" && _pending[m.requestId]) {
      _pending[m.requestId]({ status: m.status, body: m.body });
      delete _pending[m.requestId];
    }
  });
  function apiFetch(path, opts) {
    opts = opts || {};
    return new Promise(function (resolve) {
      var id = ++_seq;
      _pending[id] = resolve;
      window.parent.postMessage(
        { type: "plugin-ui-fetch", requestId: id, method: opts.method || "GET", path: path, body: opts.body },
        "*"
      );
    });
  }
  function getJSON(path) { return apiFetch(path).then(parseRes); }
  function postJSON(path, obj) { return apiFetch(path, { method: "POST", body: JSON.stringify(obj) }).then(parseRes); }
  function parseRes(res) {
    var body = {};
    try { body = res.body ? JSON.parse(res.body) : {}; } catch (_e) { body = { error: "The server sent a response this page could not read." }; }
    if (res.status >= 400 || (body && body.error)) {
      throw new Error(body && body.error ? body.error : "The request failed (HTTP " + res.status + ").");
    }
    return body;
  }

  var API = "/api/plugin-ui/linux-app-manager";
  var POLL_MS = 2000;
  var $ = function (id) { return document.getElementById(id); };
  function el(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text != null) n.textContent = text;
    return n;
  }
  function clear(n) { while (n.firstChild) n.removeChild(n.firstChild); }

  var state = {
    targets: [], byId: {}, current: null,
    overview: null, rows: {}, watching: {},
    keys: [], keysLoaded: false,
    editing: null, logApp: null, timer: null,
    lastFocus: null, confirmAction: null
  };

  // ── targets ────────────────────────────────────────────────────────
  function loadTargets() {
    return getJSON(API + "/targets").then(function (d) {
      state.targets = d.targets || [];
      state.byId = {};
      state.targets.forEach(function (t) { state.byId[t.id] = t; });
      if (!state.current || !state.byId[state.current]) {
        state.current = state.targets.length ? state.targets[0].id : null;
      }
      renderTargetPicker();
      return loadApps();
    }).catch(function (e) { fail("Could not load targets. " + e.message); });
  }

  function renderTargetPicker() {
    var sel = $("targetSel");
    clear(sel);
    state.targets.forEach(function (t) {
      var o = el("option", null, t.label);
      o.value = t.id;
      sel.appendChild(o);
    });
    if (state.current) sel.value = state.current;
    var t = state.byId[state.current];
    $("targetDetail").textContent = t ? t.detail : "";
    var remote = !!t && t.kind === "remote";
    $("editTargetBtn").disabled = !remote;
    $("removeTargetBtn").disabled = !remote;
  }

  // ── app grid ───────────────────────────────────────────────────────
  function loadApps() {
    if (!state.current) return Promise.resolve();
    var target = state.current;
    setBanner("Checking " + (state.byId[target] ? state.byId[target].label : target) + "…", "");
    clear($("grid"));
    state.rows = {};
    $("gridEmpty").textContent = "Checking which applications are installed…";
    $("gridEmpty").style.display = "block";
    return getJSON(API + "/apps?target=" + encodeURIComponent(target)).then(function (d) {
      if (state.current !== target) return; // the user switched while we waited
      state.overview = d;
      renderBanner(d.distro);
      renderGrid(d.apps || []);
    }).catch(function (e) {
      if (state.current !== target) return;
      setBanner(e.message, "bad");
      clear($("grid"));
      $("gridEmpty").textContent = "No applications could be listed for this target.";
      $("gridEmpty").style.display = "block";
    });
  }

  function setBanner(text, cls) {
    $("banner").className = "banner" + (cls ? " " + cls : "");
    $("bannerText").textContent = text;
  }

  function renderBanner(distro) {
    if (!distro) { setBanner("", ""); return; }
    if (!distro.supported) { setBanner(distro.refusal || distro.summary, "bad"); return; }
    setBanner("Detected: " + distro.summary, distro.package_manager ? "" : "warn");
  }

  function renderGrid(apps) {
    var grid = $("grid");
    clear(grid);
    state.rows = {};
    if (!apps.length) {
      $("gridEmpty").textContent = state.overview && state.overview.distro && !state.overview.distro.supported
        ? "This target is not a supported Linux host, so no applications are listed."
        : "The catalog is empty.";
      $("gridEmpty").style.display = "block";
      return;
    }
    $("gridEmpty").style.display = "none";
    apps.forEach(function (a) { grid.appendChild(buildRow(a)); });
  }

  function buildRow(a) {
    var row = el("div", "approw " + (a.installed ? "installed" : "missing"));
    var body = el("div", "body");
    var name = el("div", "name");
    name.appendChild(document.createTextNode(a.name));
    var badge = el("span", "badge", a.state_label);
    if (a.installed) badge.className = "badge ok";
    name.appendChild(badge);
    body.appendChild(name);
    body.appendChild(el("div", "desc", a.description));
    var ver = el("div", "ver", a.version || "");
    body.appendChild(ver);
    row.appendChild(body);

    var acts = el("div", "acts");
    var btn = el("button", a.action === "remove" ? "danger" : "primary", a.action_label);
    btn.setAttribute("aria-label", a.action_label + " " + a.name + " on " + targetLabel());
    btn.onclick = function () { a.action === "remove" ? askRemove(a) : startInstall(a); };
    var why = el("div", "why", "");
    if (!a.actionable) {
      btn.disabled = true;
      why.textContent = a.blocked_reason || "";
    }
    var logLink = el("button", null, "View log");
    logLink.style.display = "none";
    logLink.onclick = function () { showLog(a.id); };
    acts.appendChild(btn);
    acts.appendChild(logLink);
    acts.appendChild(why);
    row.appendChild(acts);

    state.rows[a.id] = { app: a, row: row, badge: badge, ver: ver, btn: btn, why: why, logLink: logLink };
    if (a.job) applyJob(a.id, a.job);
    return row;
  }

  function targetLabel() {
    var t = state.byId[state.current];
    return t ? t.label : (state.current || "the target");
  }

  // Reflect a job's state onto its row (and the log panel when it's showing).
  // While a job runs the badge IS the job ("Installing…"); once it settles the
  // badge goes back to reporting installed state — the outcome belongs to the
  // log panel, not to the row's at-a-glance status.
  function applyJob(appId, job) {
    var r = state.rows[appId];
    if (!r) return;
    r.job = job;
    r.logLink.style.display = job ? "" : "none";
    var running = !!job && job.status === "running";
    if (running) {
      r.badge.textContent = job.label;
      r.badge.className = "badge busy";
      r.why.textContent = "";
      if (!state.watching[appId]) watch(appId);
    } else {
      r.badge.textContent = r.app.state_label;
      r.badge.className = "badge" + (r.app.installed ? " ok" : "");
    }
    if (job) {
      r.logLink.textContent = job.status === "running" ? "View log" : "Last log";
      r.logLink.title = job.label;
    }
    r.btn.disabled = running || !r.app.actionable;
    if (job && state.logApp === appId) renderLog(job);
  }

  // ── install / remove ───────────────────────────────────────────────
  function startInstall(a) {
    var target = state.current;
    var r = state.rows[a.id];
    if (r) r.btn.disabled = true;
    openLog(a.id, "Installing " + a.name + " on " + targetLabel());
    postJSON(API + "/install", { target: target, app: a.id })
      .then(function () { watch(a.id); })
      .catch(function (e) { failRow(a, e); });
  }

  function askRemove(a) {
    confirmDialog(
      "Remove " + a.name + "?",
      a.name + " will be removed from " + targetLabel() + ".",
      "This runs a package-manager removal command AS ROOT (via sudo) on the target. " +
        "Anything that depends on " + a.name + " may stop working.",
      "Remove " + a.name,
      function () {
        var r = state.rows[a.id];
        if (r) r.btn.disabled = true;
        openLog(a.id, "Removing " + a.name + " from " + targetLabel());
        postJSON(API + "/remove", { target: state.current, app: a.id })
          .then(function () { watch(a.id); })
          .catch(function (e) { failRow(a, e); });
      }
    );
  }

  function failRow(a, e) {
    var r = state.rows[a.id];
    if (r) {
      r.btn.disabled = !r.app.actionable;
      r.badge.textContent = "Failed to start";
      r.badge.className = "badge bad";
    }
    if (state.logApp === a.id) {
      $("logStatus").textContent = "Failed to start";
      $("logStatus").className = "badge bad";
      $("logTail").textContent = e.message;
    }
    toast(e.message, true);
  }

  // ── live polling ───────────────────────────────────────────────────
  // Installs are detached jobs, so nothing here ever blocks: we kick the job
  // off, then poll /status for every app we're watching until it settles.
  function watch(appId) {
    state.watching[appId] = true;
    if (!state.timer) state.timer = setInterval(tick, POLL_MS);
    tick();
  }

  function tick() {
    var ids = Object.keys(state.watching);
    if (!ids.length) {
      clearInterval(state.timer);
      state.timer = null;
      return;
    }
    ids.forEach(function (appId) {
      var target = state.current;
      getJSON(API + "/status?target=" + encodeURIComponent(target) + "&app=" + encodeURIComponent(appId))
        .then(function (d) {
          if (state.current !== target) return;
          var r = state.rows[appId];
          if (r && d.row) {
            r.app = d.row;
            r.ver.textContent = d.row.version || "";
            r.btn.textContent = d.row.action_label;
            r.btn.className = d.row.action === "remove" ? "danger" : "primary";
            r.btn.setAttribute("aria-label", d.row.action_label + " " + d.row.name + " on " + targetLabel());
            r.btn.onclick = function () { d.row.action === "remove" ? askRemove(r.app) : startInstall(r.app); };
          }
          applyJob(appId, d.job);
          if (!d.job || d.job.status !== "running") {
            delete state.watching[appId];
            settled(appId, d);
          }
        })
        .catch(function (_e) { /* transient — keep polling */ });
    });
  }

  function settled(appId, d) {
    var r = state.rows[appId];
    var name = r ? r.app.name : appId;
    if (d.job) toast(name + ": " + d.job.label);
    // Re-read the whole target so installed state / versions / actions are
    // rebuilt from the source of truth rather than patched in place.
    loadApps().then(function () {
      if (state.logApp === appId && d.job) renderLog(d.job);
    });
  }

  // ── log panel ──────────────────────────────────────────────────────
  function openLog(appId, title) {
    state.logApp = appId;
    $("logPanel").hidden = false;
    $("logTitle").textContent = title;
    $("logStatus").textContent = "Starting…";
    $("logStatus").className = "badge busy";
    $("logTail").textContent = "(waiting for output…)";
  }

  function showLog(appId) {
    var r = state.rows[appId];
    if (!r || !r.job) return;
    var verb = r.job.action === "install" ? "Installing " : "Removing ";
    openLog(appId, verb + r.app.name + " on " + targetLabel());
    renderLog(r.job);
  }

  function renderLog(job) {
    $("logStatus").textContent = job.label;
    $("logStatus").className = "badge " + job.tone;
    var pre = $("logTail");
    var atBottom = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 30;
    pre.textContent = job.log_tail || "(no output yet)";
    if (atBottom) pre.scrollTop = pre.scrollHeight;
  }

  // ── dialogs (focus in, Escape out, focus restored) ─────────────────
  function openDialog(backdropId, firstFieldId) {
    state.lastFocus = document.activeElement;
    $(backdropId).classList.add("open");
    var f = $(firstFieldId);
    if (f) f.focus();
  }
  function closeDialog(backdropId) {
    $(backdropId).classList.remove("open");
    if (state.lastFocus && state.lastFocus.focus) state.lastFocus.focus();
    state.lastFocus = null;
  }
  document.addEventListener("keydown", function (e) {
    if (e.key !== "Escape") return;
    if ($("targetBackdrop").classList.contains("open")) closeDialog("targetBackdrop");
    else if ($("confirmBackdrop").classList.contains("open")) closeDialog("confirmBackdrop");
  });

  function confirmDialog(title, bodyText, warnText, okLabel, onOk) {
    $("confirmTitle").textContent = title;
    $("confirmBody").textContent = bodyText;
    $("confirmWarn").textContent = warnText;
    $("confirmOk").textContent = okLabel;
    state.confirmAction = onOk;
    openDialog("confirmBackdrop", "confirmCancel");
  }

  // ── add / edit remote target ───────────────────────────────────────
  // The vault keys are a fixed option set from core, so the picker is a plain
  // <select> — never free text, and metadata only (no key material).
  function loadKeys() {
    if (state.keysLoaded) return Promise.resolve(state.keys);
    return getJSON(API + "/ssh-keys").then(function (d) {
      state.keys = d.keys || [];
      state.keysLoaded = true;
      return state.keys;
    }).catch(function (e) {
      state.keys = [];
      $("keyHint").textContent = "Could not load the SSH key vault: " + e.message;
      return state.keys;
    });
  }

  function fillKeySelect(selectedId) {
    var sel = $("f_key_id");
    clear(sel);
    if (!state.keys.length) {
      var none = el("option", null, "No keys in the vault — add one in Settings → SSH Keys");
      none.value = "";
      sel.appendChild(none);
      sel.disabled = true;
      return;
    }
    sel.disabled = false;
    state.keys.forEach(function (k) {
      var o = el("option", null, k.name + "  (" + k.key_type + ")");
      o.value = k.id;
      sel.appendChild(o);
    });
    // A target may point at a key since deleted from the vault: keep it
    // selectable so saving doesn't silently re-point the target.
    if (selectedId && !state.keys.some(function (k) { return k.id === selectedId; })) {
      var missing = el("option", null, "(key no longer in the vault: " + selectedId + ")");
      missing.value = selectedId;
      sel.appendChild(missing);
    }
    sel.value = selectedId || state.keys[0].id;
  }

  function openTargetModal(existing) {
    state.editing = existing ? existing.id : null;
    $("targetModalTitle").textContent = existing ? "Edit remote target" : "Add remote target";
    $("f_label").value = existing ? existing.label : "";
    $("f_hostname").value = existing ? (existing.hostname || "") : "";
    $("f_port").value = existing ? String(existing.port || 22) : "22";
    $("f_username").value = existing ? (existing.username || "") : "";
    $("f_known").value = existing ? (existing.known_host || "") : "";
    $("targetErr").textContent = "";
    openDialog("targetBackdrop", "f_hostname");
    loadKeys().then(function () { fillKeySelect(existing ? existing.key_id : null); });
  }

  function saveTarget() {
    var body = {
      label: $("f_label").value,
      hostname: $("f_hostname").value,
      port: $("f_port").value,
      username: $("f_username").value,
      key_id: $("f_key_id").value,
      known_host: $("f_known").value
    };
    if (state.editing) body.id = state.editing;
    var btn = $("targetSave");
    btn.disabled = true;
    postJSON(API + "/targets", body).then(function (d) {
      closeDialog("targetBackdrop");
      state.current = d.target ? d.target.id : state.current;
      toast("Saved " + (d.target ? d.target.label : "target"));
      loadTargets();
    }).catch(function (e) {
      $("targetErr").textContent = e.message;
    }).then(function () { btn.disabled = false; });
  }

  function removeTarget() {
    var t = state.byId[state.current];
    if (!t || t.kind !== "remote") return;
    confirmDialog(
      "Remove target " + t.label + "?",
      "Peckboard will forget " + t.detail + ".",
      "Nothing is uninstalled and nothing is changed on the host itself — only this target entry is deleted.",
      "Remove target",
      function () {
        postJSON(API + "/target-remove", { id: t.id }).then(function () {
          state.current = null;
          toast("Removed " + t.label);
          loadTargets();
        }).catch(function (e) { toast(e.message, true); });
      }
    );
  }

  // ── toast ──────────────────────────────────────────────────────────
  var toastTimer = null;
  function toast(msg, bad) {
    var t = $("toast");
    t.textContent = msg;
    t.className = "toast open" + (bad ? " bad" : "");
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { t.className = "toast"; }, 8000);
  }
  function fail(msg) {
    setBanner(msg, "bad");
    toast(msg, true);
  }

  // ── wire up ────────────────────────────────────────────────────────
  $("targetSel").onchange = function () {
    state.current = $("targetSel").value;
    state.watching = {};
    state.logApp = null;
    $("logPanel").hidden = true;
    renderTargetPicker();
    loadApps();
  };
  $("refreshBtn").onclick = function () { loadTargets(); };
  $("addTargetBtn").onclick = function () { openTargetModal(null); };
  $("editTargetBtn").onclick = function () { openTargetModal(state.byId[state.current]); };
  $("removeTargetBtn").onclick = removeTarget;
  $("targetCancel").onclick = function () { closeDialog("targetBackdrop"); };
  $("targetSave").onclick = saveTarget;
  $("confirmCancel").onclick = function () { closeDialog("confirmBackdrop"); };
  $("confirmOk").onclick = function () {
    var fn = state.confirmAction;
    state.confirmAction = null;
    closeDialog("confirmBackdrop");
    if (fn) fn();
  };
  $("logClose").onclick = function () { state.logApp = null; $("logPanel").hidden = true; };
  $("targetBackdrop").addEventListener("mousedown", function (e) {
    if (e.target === $("targetBackdrop")) closeDialog("targetBackdrop");
  });
  $("confirmBackdrop").addEventListener("mousedown", function (e) {
    if (e.target === $("confirmBackdrop")) closeDialog("confirmBackdrop");
  });

  loadTargets();
})();
</script>
</body>
</html>`;
