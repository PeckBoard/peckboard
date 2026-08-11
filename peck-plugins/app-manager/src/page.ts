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
  /* deep-link request bar (see deeplink.ts) */
  .reqbar {
    padding: 8px 16px; border-bottom: 1px solid var(--line); background: var(--badge-bg);
    display: flex; align-items: center; gap: 12px; flex: 0 0 auto; font-size: 12px;
    flex-wrap: wrap;
  }
  .reqbar .lead { font-weight: 600; }
  .reqbar .item { display: inline-flex; align-items: center; gap: 6px; }
  .reqbar .note { color: var(--muted); }
  .approw.req { background: var(--badge-bg); }
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
  .approw .pkgver { color: var(--muted); }
  .approw .prov { margin-top: 3px; font-size: 11px; color: var(--warn); }
  .approw .deps {
    margin-top: 4px; font-size: 11px; color: var(--muted); line-height: 1.7;
  }
  .approw .deps .dep {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    white-space: nowrap;
  }
  /* dependency graph */
  .depsbar {
    padding: 6px 16px; border-bottom: 1px solid var(--line); background: var(--panel);
    display: flex; align-items: center; gap: 8px; flex: 0 0 auto; font-size: 12px; flex-wrap: wrap;
  }
  .depsbar .info { color: var(--muted); }
  .depsbar .spacer { flex: 1; }
  .depsbar select { min-width: 200px; max-width: 320px; width: auto; }
  .libpanel {
    padding: 6px 16px 10px; border-bottom: 1px solid var(--line); background: var(--panel);
    font-size: 12px; line-height: 1.8;
  }
  .mono { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
  .depstoggle { margin-top: 6px; font-size: 11px; padding: 2px 8px; }
  .depnote { margin-top: 6px; font-size: 11px; color: var(--muted); }
  .deptree { margin-top: 4px; }
  .depnode { font-size: 11px; line-height: 1.8; }
  .depnode .line { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
  .depnode .twist {
    width: 18px; height: 18px; padding: 0; font-size: 10px; line-height: 1;
    border: none; background: none; color: var(--muted); cursor: pointer;
  }
  .depnode .twist:hover:enabled { color: var(--accent); }
  .depnode .twist.leaf { visibility: hidden; } /* keeps sibling lines aligned */
  .depname { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
  .depver { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; color: var(--muted); }
  .depkind {
    font-size: 9px; border: 1px solid var(--line); border-radius: 8px; padding: 0 6px;
    color: var(--muted); background: var(--panel2); letter-spacing: .4px;
  }
  .depkind.shared { background: var(--badge-bg); border-color: var(--badge-line); color: var(--accent); }
  .libpanel .depkind { margin-right: 4px; }
  .depnode .kids { margin-left: 22px; }
  .depbins {
    margin-left: 24px; color: var(--muted); font-size: 10px;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace; word-break: break-all;
  }
  .badge {
    font-size: 10px; border-radius: 10px; padding: 1px 8px; border: 1px solid var(--line);
    background: var(--panel2); color: var(--muted); white-space: nowrap;
  }
  .badge.ok { background: var(--ok-bg); border-color: var(--ok-line); color: var(--ok); }
  .badge.busy { background: var(--badge-bg); border-color: var(--badge-line); color: var(--accent); }
  .badge.bad { background: var(--err-bg); border-color: var(--err-line); color: var(--err); }
  .badge.pip { background: var(--badge-bg); border-color: var(--badge-line); color: var(--accent); }
  .badge.manual { background: var(--warn-bg); border-color: var(--warn-line); color: var(--warn); }
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
  .sessionbar {
    display: flex; align-items: center; gap: 8px; padding: 8px 12px;
    border-bottom: 1px solid var(--line); font-size: 11px;
  }
  .sessionbar[hidden] { display: none; }
  .sessionbar .note { flex: 1; color: var(--muted); }
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
  <button id="addAppBtn">+ Add app</button>
  <button id="addTargetBtn">+ Add remote target</button>
  <button id="editTargetBtn">Edit target</button>
  <button id="removeTargetBtn" class="danger">Remove target</button>
  <button id="refreshBtn" class="primary">Refresh</button>
</header>

<div class="banner" id="banner" role="status" aria-live="polite"><span id="bannerText">Loading…</span></div>
<div class="reqbar" id="reqBar" role="status" hidden></div>
<div class="depsbar" id="depsBar">
  <span class="info" id="depsInfo">Dependencies: not resolved yet.</span>
  <span class="spacer"></span>
  <label class="sub" for="libSel">Reverse view</label>
  <select id="libSel" aria-label="Select a dependency to see which apps require it"></select>
  <button id="libSysBtn" title="Ask the target's package manager which installed packages require the selected dependency, system-wide">System-wide</button>
  <button id="depsRefreshBtn">Refresh dependencies</button>
</div>
<div class="libpanel" id="libPanel" hidden></div>
<div class="libpanel" id="pipPanel" hidden></div>

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
    <div class="sessionbar" id="sessionBar" hidden>
      <span class="note" id="sessionNote">Tool-level session activity only — event names, never command output. Open the session for the full conversation.</span>
      <button id="openSessionBtn" class="primary">Open install session</button>
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

<div class="backdrop" id="appBackdrop">
  <div class="modal" role="dialog" aria-modal="true" aria-labelledby="appModalTitle">
    <h2 id="appModalTitle">Add an app</h2>
    <div class="form">
      <p style="margin:0" class="sub">Adding an app only creates the row. Nothing is installed until you press Install on it.</p>
      <div class="field">
        <label for="f_app_name">App name *</label>
        <input id="f_app_name" placeholder="Zellij" />
        <span class="hint">What the software is called. On this host, the install session looks it up — searching the web if it doesn't know it — and installs it from an official source only.</span>
      </div>
      <div class="field">
        <label for="f_app_binary">Command that proves it is installed</label>
        <input id="f_app_binary" placeholder="zellij" />
        <span class="hint">The executable checked with <code>command -v</code>. Defaults to the app name in lowercase. This probe, not the agent's report, decides whether an install succeeded.</span>
      </div>
      <div class="field">
        <label for="f_app_home">Official website (optional)</label>
        <input id="f_app_home" placeholder="https://zellij.dev" />
        <span class="hint">https only. Treated as a claim to verify, not as a source of truth.</span>
      </div>
      <div class="field">
        <label for="f_app_notes">Notes (optional)</label>
        <input id="f_app_notes" placeholder="terminal multiplexer, Rust" />
        <span class="hint">Anything that pins down which project you mean — handed to the install session as context.</span>
      </div>
      <div class="field">
        <label for="f_app_install">Install command (required for remote targets)</label>
        <input id="f_app_install" placeholder="sudo -A apt-get install -y zellij" />
        <span class="hint">Remote targets have no AI session available, so they run this command verbatim over SSH. On this host it is only a suggestion the session checks first. Leave blank to install here only.</span>
      </div>
      <div class="field">
        <label for="f_app_remove">Remove command (optional)</label>
        <input id="f_app_remove" placeholder="sudo -A apt-get remove -y zellij" />
        <span class="hint">Run verbatim when you press Remove. Without one, the row offers Forget instead — App Manager never guesses how to uninstall something.</span>
      </div>
      <p class="warnbox" id="appCmdWarn" hidden></p>
      <p class="formerr" id="appErr"></p>
    </div>
    <div class="foot">
      <button id="appCancel">Cancel</button>
      <button id="appSave" class="primary">Save app</button>
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

<div class="backdrop" id="installBackdrop">
  <div class="modal" role="dialog" aria-modal="true" aria-labelledby="installModalTitle">
    <h2 id="installModalTitle">Install</h2>
    <div class="form">
      <p id="installIntro" style="margin:0"></p>
      <div class="field">
        <label for="f_model">Account and model *</label>
        <select id="f_model"></select>
        <span class="hint">The install runs in a temporary AI session on this account and model. Only thinking-capable models are offered; the choice is saved as the default for next time.</span>
      </div>
      <p class="formerr" id="installErr"></p>
    </div>
    <div class="foot">
      <button id="installCancel">Cancel</button>
      <button id="installStart" class="primary">Start install session</button>
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

  var API = "/api/plugin-ui/app-manager";
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
    deps: null, libs: {},
    keys: [],
    editing: null, editingApp: null, logApp: null, timer: null,
    lastFocus: null, confirmAction: null, installApp: null, sessionId: null
  };

  // What a deep link asked us to install, parsed server-side and baked in
  // (deeplink.ts). null when the page was opened normally. It PREFILLS ONLY:
  // the request bar names the apps and every install still goes through the
  // same button, the same account+model picker, and the same confirmation.
  var REQ = __APP_MANAGER_REQUEST__;
  // The requested target is honoured once, on the first load.
  var reqTargetPending = !!(REQ && REQ.target);

  function requested(appId) {
    return !!REQ && REQ.apps.indexOf(appId) >= 0;
  }

  // ── targets ────────────────────────────────────────────────────────
  function loadTargets() {
    return getJSON(API + "/targets").then(function (d) {
      state.targets = d.targets || [];
      state.byId = {};
      state.targets.forEach(function (t) { state.byId[t.id] = t; });
      if (reqTargetPending) {
        reqTargetPending = false;
        if (state.byId[REQ.target]) state.current = REQ.target;
      }
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
      renderRequest();
      loadDeps();
    }).catch(function (e) {
      if (state.current !== target) return;
      setBanner(e.message, "bad");
      clear($("grid"));
      renderRequest();
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

  // ── deep-link request bar ──────────────────────────────────────────
  // Names what the linking page asked for and offers each missing app's own
  // Install button — the SAME button the row has, so a local install still
  // opens the account+model picker and a removal is never offered here.
  function renderRequest() {
    var bar = $("reqBar");
    clear(bar);
    if (!REQ || !REQ.apps.length) { bar.hidden = true; return; }
    bar.hidden = false;

    var known = {};
    ((state.overview && state.overview.apps) || []).forEach(function (a) {
      known[a.id] = a;
    });
    bar.appendChild(el("span", "lead",
      (REQ.from ? REQ.from + " asked for these on " : "Requested on ") + targetLabel() + ":"));

    var unknown = [];
    REQ.apps.forEach(function (id) {
      var a = known[id];
      if (!a) { unknown.push(id); return; }
      var item = el("span", "item");
      item.appendChild(document.createTextNode(a.name));
      if (a.installed) {
        item.appendChild(el("span", "badge ok", a.state_label));
        item.appendChild(el("span", "note", "already installed"));
      } else {
        var btn = el("button", "primary", a.action_label);
        btn.setAttribute("aria-label", a.action_label + " " + a.name + " on " + targetLabel());
        if (!a.actionable) {
          btn.disabled = true;
          item.appendChild(el("span", "note", a.blocked_reason || ""));
        }
        btn.onclick = function () { focusRow(a.id); startInstall(a); };
        item.appendChild(btn);
      }
      bar.appendChild(item);
    });

    if (unknown.length) {
      bar.appendChild(el("span", "note",
        "Not in the catalog for this target: " + unknown.join(", ") + "."));
    }
  }

  function focusRow(appId) {
    var r = state.rows[appId];
    if (r && r.row && r.row.scrollIntoView) r.row.scrollIntoView({ block: "center" });
  }

  function buildRow(a) {
    var row = el("div", "approw " + (a.installed ? "installed" : "missing")
      + (requested(a.id) ? " req" : ""));
    var body = el("div", "body");
    var name = el("div", "name");
    name.appendChild(document.createTextNode(a.name));
    var badge = el("span", "badge", a.state_label);
    if (a.installed) badge.className = "badge ok";
    name.appendChild(badge);
    if (a.namespace === "pip") {
      // Distinct namespace marker: a pip package is not a system package.
      name.appendChild(el("span", "badge pip", "pip"));
    }
    if (a.custom) {
      // Manually added, not a vetted catalog entry — say so on the row.
      name.appendChild(el("span", "badge manual", "added by hand"));
    }
    body.appendChild(name);
    body.appendChild(el("div", "desc", a.description));
    var ver = el("div", "ver", a.version || "");
    if (a.package_version) {
      ver.appendChild(el("span", "pkgver",
        (a.version ? " — package: " : "package: ") + a.package_version));
    }
    body.appendChild(ver);
    if (a.provenance_note) body.appendChild(el("div", "prov", a.provenance_note));
    if (a.deps_note) body.appendChild(el("div", "prov", a.deps_note));
    if (a.added_packages && a.added_packages.length) {
      var deps = el("div", "deps");
      deps.appendChild(document.createTextNode((a.added_label || "Also installed") + ": "));
      a.added_packages.forEach(function (p, i) {
        if (i) deps.appendChild(document.createTextNode(" · "));
        deps.appendChild(el("span", "dep", p.name + " " + p.version));
      });
      body.appendChild(deps);
    }
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
    if (a.custom) {
      var editBtn = el("button", null, "Edit");
      editBtn.setAttribute("aria-label", "Edit " + a.name);
      editBtn.onclick = function () { openAppModal(a.id); };
      acts.appendChild(editBtn);
      var forgetBtn = el("button", null, "Forget");
      forgetBtn.setAttribute("aria-label", "Forget " + a.name);
      forgetBtn.onclick = function () { askForget(a); };
      acts.appendChild(forgetBtn);
    }
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
    var t = state.byId[state.current];
    // Local installs run through a temporary AI session — the user picks the
    // account + model first. Remote targets keep the scripted recipe (an AI
    // session runs on the Peckboard host and has no path to a remote
    // target's SSH credentials).
    if (t && t.kind === "local") { openInstallPicker(a); return; }
    // A manually added app's remote install runs the command the person typed
    // for it, verbatim — so show it back before running it.
    if (a.custom && a.action_command) {
      confirmDialog(
        "Install " + a.name + " on " + targetLabel() + "?",
        "This runs the install command stored with " + a.name + " on " + targetLabel() +
          ", exactly as written. No AI session is involved on a remote target.",
        a.action_command,
        "Run install command",
        function () { runInstall(a); }
      );
      return;
    }
    runInstall(a);
  }

  function runInstall(a) {
    var target = state.current;
    var r = state.rows[a.id];
    if (r) r.btn.disabled = true;
    openLog(a.id, "Installing " + a.name + " on " + targetLabel());
    postJSON(API + "/install", { target: target, app: a.id })
      .then(function () { watch(a.id); })
      .catch(function (e) { failRow(a, e); });
  }

  // ── install picker (account + model for the AI install session) ────
  // The option set is fixed and server-filtered to thinking-capable models
  // (GET /install-options), so this is a plain <select> — never free text.
  function accountLabel(m) {
    return m.provider + " — " + (m.account_id ? "account " + m.account_id : "default account");
  }

  function fillModelSelect(models, defaultModel) {
    var sel = $("f_model");
    clear(sel);
    var groups = {};
    models.forEach(function (m) {
      var key = accountLabel(m);
      if (!groups[key]) {
        groups[key] = el("optgroup");
        groups[key].label = key;
        sel.appendChild(groups[key]);
      }
      var o = el("option", null, m.display_name);
      o.value = m.id;
      groups[key].appendChild(o);
    });
    if (defaultModel && models.some(function (m) { return m.id === defaultModel; })) {
      sel.value = defaultModel;
    }
  }

  function openInstallPicker(a) {
    state.installApp = a;
    $("installModalTitle").textContent = "Install " + a.name;
    $("installIntro").textContent = a.custom
      ? "“" + a.name + "” was added by hand, so a temporary AI session works out how to install it " +
        "on " + targetLabel() + ": it identifies the software — searching the web if it doesn't know it — " +
        "and downloads only from the project's own official source, its official registry entry, or this " +
        "distribution's repositories. If it can't confirm an official source it stops instead of installing. " +
        "Steps that need root use sudo with Peckboard's masked password dialog, and you can watch or stop " +
        "the session from its tab at any time."
      : "A temporary AI session performs this install on " + targetLabel() +
        ". Steps that need root use sudo with Peckboard's masked password dialog. " +
        "You can watch or stop the session from its tab at any time.";
    $("installErr").textContent = "";
    var sel = $("f_model");
    clear(sel);
    sel.disabled = true;
    $("installStart").disabled = true;
    openDialog("installBackdrop", "f_model");
    getJSON(API + "/install-options").then(function (d) {
      var models = d.models || [];
      fillModelSelect(models, d.default_model || null);
      sel.disabled = !models.length;
      $("installStart").disabled = !models.length;
      if (!models.length) {
        $("installErr").textContent =
          "No thinking-capable models are available. Configure an agent provider account first.";
      }
    }).catch(function (e) { $("installErr").textContent = e.message; });
  }

  function startInstallSession() {
    var a = state.installApp;
    if (!a) return;
    var model = $("f_model").value;
    if (!model) {
      $("installErr").textContent = "Pick the account and model for the install session.";
      return;
    }
    $("installStart").disabled = true;
    postJSON(API + "/install", { target: state.current, app: a.id, model: model })
      .then(function () {
        closeDialog("installBackdrop");
        var r = state.rows[a.id];
        if (r) r.btn.disabled = true;
        openLog(a.id, "Installing " + a.name + " via AI session");
        watch(a.id);
      })
      .catch(function (e) {
        $("installStart").disabled = false;
        $("installErr").textContent = e.message;
      });
  }

  function askRemove(a) {
    var entry = depEntryFor(a.id);
    var warn = a.custom
      ? "This runs the remove command stored with " + a.name + " on the target, exactly as written: " +
        (a.action_command || "")
      : "This runs a package-manager removal command AS ROOT (via sudo) on the target. " +
        "Anything that depends on " + a.name + " may stop working.";
    // Autoremove-accurate impact from the dependency graph: what genuinely
    // becomes unneeded, and which shared dependencies stay (see deps.ts).
    if (entry && entry.removal_note) warn += " " + entry.removal_note;
    confirmDialog(
      "Remove " + a.name + "?",
      a.name + " will be removed from " + targetLabel() + ".",
      warn,
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
    $("sessionBar").hidden = true;
    state.sessionId = null;
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
    if (job.is_session) {
      // AI-session installs have no log — core exposes only event kinds and
      // tool names to plugins, never payloads. Render that activity plus the
      // session link, and never imply it is command output.
      $("sessionBar").hidden = false;
      state.sessionId = job.session_id || null;
      $("openSessionBtn").disabled = !state.sessionId;
      var lines = (job.activity || []).join("\\n");
      if (job.message) lines += (lines ? "\\n\\n" : "") + job.message;
      pre.textContent = lines || "(no session activity yet)";
    } else {
      $("sessionBar").hidden = true;
      pre.textContent = job.log_tail || "(no output yet)";
    }
    if (atBottom) pre.scrollTop = pre.scrollHeight;
  }

  // ── dependency graph ───────────────────────────────────────────────
  // Rendering only ever reads the cached graph (GET /deps — no execs on
  // the target); resolution happens when a job settles or via the explicit
  // button (POST /deps-refresh). Trees arrive as server-shaped plain data.
  function loadDeps() {
    var target = state.current;
    if (!target) return Promise.resolve();
    return getJSON(API + "/deps?target=" + encodeURIComponent(target)).then(function (d) {
      if (state.current !== target) return;
      state.deps = d;
      renderDeps();
    }).catch(function (e) {
      if (state.current !== target) return;
      state.deps = null;
      renderDeps();
      $("depsInfo").textContent = "Dependencies unavailable: " + e.message;
    });
  }

  function refreshDeps() {
    var target = state.current;
    if (!target) return;
    var btn = $("depsRefreshBtn");
    btn.disabled = true;
    btn.textContent = "Resolving…";
    $("depsInfo").textContent = "Querying the package manager on " + targetLabel() + "…";
    postJSON(API + "/deps-refresh", { target: target }).then(function (d) {
      if (state.current !== target) return;
      state.deps = d;
      renderDeps();
      toast("Dependencies resolved for " + targetLabel());
    }).catch(function (e) {
      if (state.current === target) {
        renderDeps();
        $("depsInfo").textContent = e.message;
        toast(e.message, true);
      }
    }).then(function () {
      btn.disabled = false;
      btn.textContent = "Refresh dependencies";
    });
  }

  function depEntryFor(appId) {
    var apps = state.deps && state.deps.apps ? state.deps.apps : [];
    for (var i = 0; i < apps.length; i++) {
      if (apps[i].id === appId) return apps[i];
    }
    return null;
  }

  function renderDeps() {
    var g = state.deps && state.deps.graph;
    $("depsInfo").textContent = g
      ? "Dependencies resolved " + fmtWhen(g.at) + " — depth " + g.depth + ", " +
        g.node_count + " packages, " + g.edge_count + " edges" +
        (g.truncated ? " (truncated at the size cap)" : "") + "."
      : "Dependencies: not resolved yet for this target.";
    fillLibSelect(state.deps ? state.deps.libraries || [] : []);
    renderLibPanel();
    renderPipPanel();
    Object.keys(state.rows).forEach(function (appId) { attachRowDeps(appId); });
  }

  function fillLibSelect(libs) {
    var sel = $("libSel");
    var prev = sel.value;
    clear(sel);
    state.libs = {};
    if (!libs.length) {
      var none = el("option", null, "No dependencies resolved yet");
      none.value = "";
      sel.appendChild(none);
      sel.disabled = true;
      $("libSysBtn").disabled = true;
      return;
    }
    sel.disabled = false;
    var ph = el("option", null, "Select a library or package…");
    ph.value = "";
    sel.appendChild(ph);
    libs.forEach(function (l) {
      state.libs[l.name] = l;
      var o = el("option", null,
        l.name + (l.version ? " " + l.version : "") + (l.shared ? " — shared" : ""));
      o.value = l.name;
      sel.appendChild(o);
    });
    sel.value = prev && state.libs[prev] ? prev : "";
    $("libSysBtn").disabled = !sel.value;
  }

  function renderLibPanel() {
    var panel = $("libPanel");
    clear(panel);
    var name = $("libSel").value;
    var entry = name ? state.libs[name] : null;
    $("libSysBtn").disabled = !entry;
    if (!entry) { panel.hidden = true; return; }
    panel.hidden = false;
    var line = el("div", null, "");
    line.appendChild(el("span", "mono", entry.name + (entry.version ? " " + entry.version : "")));
    line.appendChild(document.createTextNode(" "));
    line.appendChild(el("span", "depkind", entry.kind));
    if (entry.shared) line.appendChild(el("span", "depkind shared", "shared"));
    line.appendChild(document.createTextNode(
      entry.required_by && entry.required_by.length
        ? " Required by " + entry.required_by.join(", ") + "."
        : " No catalog app requires it — it arrived as part of the wider dependency set."
    ));
    panel.appendChild(line);
    var sys = el("div", null, "");
    sys.id = "libSysOut";
    panel.appendChild(sys);
  }

  // pip namespace: its own block, never mixed into the system package
  // graph above — a pip package is not a distro package.
  function renderPipPanel() {
    var panel = $("pipPanel");
    clear(panel);
    var pkgs = state.deps && state.deps.pip_packages ? state.deps.pip_packages : [];
    if (!pkgs.length) { panel.hidden = true; return; }
    panel.hidden = false;
    var head = el("div", null, "");
    head.appendChild(el("b", null, "Python packages (pip)"));
    head.appendChild(document.createTextNode(
      " — pip's own namespace, separate from the system packages above."));
    panel.appendChild(head);
    pkgs.forEach(function (p) {
      var line = el("div", null, "");
      line.appendChild(el("span", "mono", p.name + (p.version ? " " + p.version : "")));
      line.appendChild(el("span", "badge pip", "pip"));
      var bits = [];
      if (p.requires && p.requires.length) bits.push("requires: " + p.requires.join(", "));
      if (p.required_by && p.required_by.length) bits.push("required by: " + p.required_by.join(", "));
      line.appendChild(document.createTextNode(
        bits.length ? " — " + bits.join("; ") : " — no pip dependencies reported"));
      panel.appendChild(line);
    });
  }
  function sysRdeps() {
    var name = $("libSel").value;
    var target = state.current;
    var out = $("libSysOut");
    if (!name || !out) return;
    out.textContent = "Asking the package manager…";
    getJSON(API + "/rdeps?target=" + encodeURIComponent(target) + "&pkg=" + encodeURIComponent(name))
      .then(function (d) {
        if (state.current !== target || $("libSel").value !== name) return;
        var list = d.required_by || [];
        if (!list.length) {
          out.textContent = "System-wide: " + (d.note || "no installed package declares a dependency on it.");
          return;
        }
        var shown = list.slice(0, 30).join(", ");
        var more = list.length > 30 ? " and " + (list.length - 30) + " more" : "";
        out.textContent = "System-wide, " + list.length + " installed package" +
          (list.length === 1 ? " requires" : "s require") + " it: " + shown + more + ".";
      })
      .catch(function (e) { out.textContent = "System-wide query failed: " + e.message; });
  }

  function attachRowDeps(appId) {
    var r = state.rows[appId];
    if (!r) return;
    if (r.depsEl && r.depsEl.parentNode) r.depsEl.parentNode.removeChild(r.depsEl);
    r.depsEl = null;
    if (!r.app.installed) return;
    var entry = depEntryFor(appId);
    var box = el("div", "depsblock");
    if (entry && entry.tree && entry.tree.length) {
      var btn = el("button", "depstoggle", "Dependencies ▸");
      btn.setAttribute("aria-expanded", "false");
      var tree = el("div", "deptree");
      tree.hidden = true;
      var built = false;
      btn.onclick = function () {
        if (!built) {
          entry.tree.forEach(function (n) { tree.appendChild(depNodeEl(n, 0)); });
          built = true;
        }
        tree.hidden = !tree.hidden;
        btn.textContent = tree.hidden ? "Dependencies ▸" : "Dependencies ▾";
        btn.setAttribute("aria-expanded", tree.hidden ? "false" : "true");
      };
      box.appendChild(btn);
      box.appendChild(tree);
    } else if (entry && entry.note) {
      // e.g. a vendor curl|sh install: "not tracked by the package manager",
      // stated plainly instead of an empty tree that reads as "no deps".
      box.appendChild(el("div", "depnote", entry.note));
    } else {
      box.appendChild(el("div", "depnote", "Dependencies not resolved yet."));
    }
    var body = r.row.querySelector(".body");
    if (body) { body.appendChild(box); r.depsEl = box; }
  }

  // One dependency line: name + version + kind, shared flagged, children
  // behind the twist. The app's own package opens pre-expanded so its direct
  // dependencies are one click away, not two; everything deeper is collapsed.
  function depNodeEl(n, depth) {
    var wrap = el("div", "depnode");
    wrap.setAttribute("data-dep", n.name);
    var line = el("div", "line");
    var hasKids = n.children && n.children.length;
    var twist = el("button", "twist" + (hasKids ? "" : " leaf"), "▸");
    var kids = null;
    if (hasKids) {
      kids = el("div", "kids");
      kids.hidden = true;
      var built = false;
      var toggleKids = function () {
        if (!built) {
          n.children.forEach(function (c) { kids.appendChild(depNodeEl(c, depth + 1)); });
          built = true;
        }
        kids.hidden = !kids.hidden;
        twist.textContent = kids.hidden ? "▸" : "▾";
      };
      twist.setAttribute("aria-label", "Expand " + n.name);
      twist.onclick = toggleKids;
      if (depth === 0) toggleKids();
    } else {
      twist.disabled = true;
      twist.tabIndex = -1;
    }
    line.appendChild(twist);
    line.appendChild(el("span", "depname", n.name));
    if (n.version) line.appendChild(el("span", "depver", n.version));
    line.appendChild(el("span", "depkind", n.kind));
    if (n.shared) line.appendChild(el("span", "depkind shared", "shared"));
    wrap.appendChild(line);
    if (n.binaries && n.binaries.length) {
      wrap.appendChild(el("div", "depbins", n.binaries.join("  ")));
    }
    if (kids) wrap.appendChild(kids);
    return wrap;
  }

  function fmtWhen(iso) {
    var d = new Date(iso);
    return isNaN(d.getTime()) ? (iso || "") : d.toLocaleString();
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
    else if ($("appBackdrop").classList.contains("open")) closeDialog("appBackdrop");
    else if ($("installBackdrop").classList.contains("open")) closeDialog("installBackdrop");
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
    // Always refetch. The vault is edited elsewhere (Settings → SSH Keys), so
    // a cached list goes stale the moment a key is added or deleted and the
    // picker would offer a key that no longer exists — or omit the one the
    // user just created.
    return getJSON(API + "/ssh-keys").then(function (d) {
      state.keys = d.keys || [];
      $("keyHint").textContent = "";
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
    // Saving before the picker is populated would post an empty key_id and
    // come back "key_id is required", so hold Save until the keys land.
    $("targetSave").disabled = true;
    loadKeys().then(function () {
      fillKeySelect(existing ? existing.key_id : null);
      $("targetSave").disabled = false;
    });
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

  // ── add / edit / forget a manually added app ───────────────────────
  // Free text is right here: an app's name and its install command are the
  // person's own words, not a choice from a known set. What the plugin
  // derives from them (the detect/version probes) is built from a validated
  // token server-side and shell-quoted; the commands are stored verbatim and
  // shown back verbatim before they run.
  function openAppModal(id) {
    state.editingApp = id || null;
    $("appModalTitle").textContent = id ? "Edit app" : "Add an app";
    $("appSave").textContent = id ? "Save changes" : "Save app";
    $("appErr").textContent = "";
    $("appCmdWarn").hidden = true;
    $("appCmdWarn").textContent = "";
    ["f_app_name", "f_app_binary", "f_app_home", "f_app_notes", "f_app_install", "f_app_remove"]
      .forEach(function (f) { $(f).value = ""; });
    $("f_app_name").disabled = !!id;
    openDialog("appBackdrop", id ? "f_app_binary" : "f_app_name");
    if (!id) return;
    $("appSave").disabled = true;
    getJSON(API + "/apps-custom").then(function (d) {
      var rec = null;
      (d.apps || []).forEach(function (r) { if (r.id === id) rec = r; });
      if (!rec) { $("appErr").textContent = "That app is no longer in the list."; return; }
      $("f_app_name").value = rec.name || "";
      $("f_app_binary").value = rec.binary || "";
      $("f_app_home").value = rec.homepage || "";
      $("f_app_notes").value = rec.notes || "";
      $("f_app_install").value = rec.install_command || "";
      $("f_app_remove").value = rec.remove_command || "";
    }).catch(function (e) {
      $("appErr").textContent = e.message;
    }).then(function () { $("appSave").disabled = false; });
  }

  function saveApp() {
    var body = {
      name: $("f_app_name").value,
      binary: $("f_app_binary").value,
      homepage: $("f_app_home").value,
      notes: $("f_app_notes").value,
      install_command: $("f_app_install").value,
      remove_command: $("f_app_remove").value
    };
    if (state.editingApp) body.id = state.editingApp;
    var btn = $("appSave");
    btn.disabled = true;
    postJSON(API + "/apps-custom", body).then(function (d) {
      closeDialog("appBackdrop");
      state.editingApp = null;
      toast("Saved " + (d.app ? d.app.name : "app") + ". Nothing is installed until you press Install.");
      loadApps();
    }).catch(function (e) {
      $("appErr").textContent = e.message;
    }).then(function () { btn.disabled = false; });
  }

  // Forget is not an uninstall, and the confirmation says exactly that.
  function askForget(a) {
    confirmDialog(
      "Forget " + a.name + "?",
      a.name + " is removed from App Manager's list of apps.",
      "Nothing is uninstalled. If " + a.name + " is installed on " + targetLabel() +
        ", it stays installed — this only deletes the entry here.",
      "Forget " + a.name,
      function () {
        postJSON(API + "/apps-custom-remove", { id: a.id }).then(function () {
          toast("Forgot " + a.name + ". Nothing was uninstalled.");
          loadApps();
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
    state.deps = null;
    state.libs = {};
    renderDeps();
    state.logApp = null;
    $("logPanel").hidden = true;
    renderTargetPicker();
    loadApps();
  };
  $("refreshBtn").onclick = function () { loadTargets(); };
  $("depsRefreshBtn").onclick = refreshDeps;
  $("libSel").onchange = renderLibPanel;
  $("libSysBtn").onclick = sysRdeps;
  $("addTargetBtn").onclick = function () { openTargetModal(null); };
  $("addAppBtn").onclick = function () { openAppModal(null); };
  $("appCancel").onclick = function () { closeDialog("appBackdrop"); };
  $("appSave").onclick = saveApp;
  $("appBackdrop").addEventListener("mousedown", function (e) {
    if (e.target === $("appBackdrop")) closeDialog("appBackdrop");
  });
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
  $("installCancel").onclick = function () { closeDialog("installBackdrop"); };
  $("installStart").onclick = startInstallSession;
  $("openSessionBtn").onclick = function () {
    if (!state.sessionId) return;
    // Handled by the host page (PluginFullPage): dispatches the same
    // peckboard:open-session event the core install flow uses, so the
    // session tab opens in the main app.
    window.parent.postMessage(
      { type: "plugin-ui-open-session", sessionId: state.sessionId },
      "*"
    );
  };
  $("installBackdrop").addEventListener("mousedown", function (e) {
    if (e.target === $("installBackdrop")) closeDialog("installBackdrop");
  });
  $("confirmBackdrop").addEventListener("mousedown", function (e) {
    if (e.target === $("confirmBackdrop")) closeDialog("confirmBackdrop");
  });

  loadTargets();
})();
</script>
</body>
</html>`;
