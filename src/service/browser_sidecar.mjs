// PeckBoard browser capture sidecar.
//
// Spawned by `service/browser.rs` as:
//   npx -y -p better-playwright-mcp3@<pinned> node <this file>
// (env: PORT, HEADLESS=true, NO_USER_PROFILE=true)
//
// It runs the UNMODIFIED upstream PlaywrightServer (same routes, same
// behavior the `browser_*` tools already rely on) and adds the one thing
// upstream lacks: capture. Every page created gets Playwright
// request/response/console listeners feeding a per-page ring buffer, served
// at `GET /api/pages/:pageId/events?since=<seq>` — which PeckBoard core
// polls after each recorded step (masking happens core-side, before disk).
//
// If locating or patching the upstream package fails, it falls back to
// exec'ing the plain upstream server binary: browsing keeps working,
// capture is simply absent.

import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { existsSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, delimiter } from "node:path";
import { pathToFileURL } from "node:url";

const PKG = "better-playwright-mcp3";
const PORT = parseInt(process.env.PORT || "3111", 10);

// Upstream ignores NO_USER_PROFILE and defaults every instance to ONE
// shared Chrome profile (~/.better-playwright-mcp/user-data) — a second
// peckboard (or any other consumer, or an orphaned Chrome from a killed
// predecessor) then dies on Chrome's ProcessSingleton lock. Honor the flag
// here: a truly ephemeral per-process profile unless the caller pinned
// USER_DATA_DIR explicitly.
if (process.env.NO_USER_PROFILE === "true" && !process.env.USER_DATA_DIR) {
  const dir = join(
    tmpdir(),
    "peckboard-browser-profile-" + PORT + "-" + process.pid,
  );
  try {
    mkdirSync(dir, { recursive: true });
    process.env.USER_DATA_DIR = dir;
  } catch {
    /* fall through to the upstream default */
  }
}

// Ring-buffer caps (masking and persistence caps live in core).
const MAX_BUFFERED_EVENTS = 2000;
const BODY_READ_LIMIT = 262144; // skip body reads beyond 256 KB
const BODY_KEEP_CHARS = 8192;
const TEXTY_CONTENT = /json|text|xml|x-www-form-urlencoded|javascript/i;

/** pageId -> { seq, dropped, events: [{seq, kind, ...}] } */
const buffers = new Map();

function buffer(pageId) {
  let b = buffers.get(pageId);
  if (!b) {
    b = { seq: 0, dropped: 0, events: [], nextReqId: 0 };
    buffers.set(pageId, b);
  }
  return b;
}

function push(b, event) {
  event.seq = ++b.seq;
  b.events.push(event);
  if (b.events.length > MAX_BUFFERED_EVENTS) {
    b.events.shift();
    b.dropped++;
  }
}

function attachCapture(pageId, page) {
  const b = buffer(pageId);
  /** live request -> {id, ts} (playwright Request objects are identity-stable) */
  const inflight = new Map();

  page.on("request", (req) => {
    try {
      const id = ++b.nextReqId;
      inflight.set(req, id);
      let postData = null;
      try {
        postData = req.postData();
      } catch {
        /* binary or unavailable */
      }
      push(b, {
        kind: "net-req",
        id,
        ts: Date.now(),
        method: req.method(),
        url: req.url(),
        resourceType: req.resourceType(),
        headers: req.headers(),
        ...(postData != null && {
          postData: postData.slice(0, BODY_KEEP_CHARS),
        }),
      });
    } catch {
      /* capture must never break the page */
    }
  });

  page.on("requestfinished", async (req) => {
    const id = inflight.get(req);
    if (id === undefined) return;
    inflight.delete(req);
    try {
      const resp = await req.response();
      if (!resp) return;
      const headers = resp.headers();
      const size = parseInt(headers["content-length"] || "", 10);
      let body;
      let bodyTruncated = false;
      const ct = headers["content-type"] || "";
      if (TEXTY_CONTENT.test(ct) && !(size > BODY_READ_LIMIT)) {
        try {
          const buf = await resp.body();
          if (buf.length <= BODY_READ_LIMIT) {
            const text = buf.toString("utf8");
            body = text.slice(0, BODY_KEEP_CHARS);
            bodyTruncated = text.length > BODY_KEEP_CHARS;
          }
        } catch {
          /* body unavailable (redirect/navigation teardown) */
        }
      }
      push(b, {
        kind: "net-fin",
        id,
        ts: Date.now(),
        status: resp.status(),
        headers,
        ...(body !== undefined && { body, bodyTruncated }),
        ...(Number.isFinite(size) && { size }),
      });
    } catch {
      /* ignore */
    }
  });

  page.on("requestfailed", (req) => {
    const id = inflight.get(req);
    if (id === undefined) return;
    inflight.delete(req);
    try {
      push(b, {
        kind: "net-fin",
        id,
        ts: Date.now(),
        failure: (req.failure() || {}).errorText || "failed",
      });
    } catch {
      /* ignore */
    }
  });

  page.on("console", (msg) => {
    try {
      push(b, {
        kind: "console",
        ts: Date.now(),
        level: msg.type(),
        text: String(msg.text()).slice(0, 4000),
      });
    } catch {
      /* ignore */
    }
  });

  page.on("pageerror", (err) => {
    try {
      push(b, {
        kind: "console",
        ts: Date.now(),
        level: "error",
        text: ("Uncaught " + String(err)).slice(0, 4000),
      });
    } catch {
      /* ignore */
    }
  });

  // Keep the buffer briefly after close so core's final drain still works.
  page.on("close", () => {
    setTimeout(() => buffers.delete(pageId), 120000).unref?.();
  });
}

/**
 * Cursor replay capture. An init script reports throttled mousemove plus
 * mousedown from the page via an exposed binding; Playwright's own
 * high-level actions (click/hover) drive the real mouse, so agent actions
 * show up too. Coordinates are CSS px in a vw×vh viewport (the player
 * normalizes, so DPR never matters).
 *
 * The binding is page-global while runs are keyed by pageId — resolved
 * lazily through `pageIds` (filled in the pages.set patch, which runs
 * before the initial goto can emit anything).
 */
const POINTER_THROTTLE_MS = 60;
/** Page object -> upstream pageId (set in the pages.set patch). */
const pageIds = new WeakMap();

/** Install the binding + init script; MUST be awaited before the first
 * goto — fire-and-forget loses the race with the initial navigation and
 * the loaded document never gets its listeners. */
async function installPointer(page) {
  try {
    await page.exposeBinding("__peckboardPointer", (source, ev) => {
      try {
        const pageId = pageIds.get(source.page);
        if (pageId === undefined || !ev || typeof ev !== "object") return;
        push(buffer(pageId), {
          kind: "pointer",
          ts: Date.now(),
          t: ev.t === "down" ? "down" : "move",
          x: Math.round(Number(ev.x) || 0),
          y: Math.round(Number(ev.y) || 0),
          vw: Math.round(Number(ev.vw) || 0),
          vh: Math.round(Number(ev.vh) || 0),
        });
      } catch {
        /* capture must never break the page */
      }
    });
    await page.addInitScript(`(() => {
      if (window.__peckboardPointerHooked) return;
      window.__peckboardPointerHooked = true;
      var last = 0;
      var send = function (t, e) {
        try {
          window.__peckboardPointer({
            t: t, x: e.clientX, y: e.clientY,
            vw: window.innerWidth, vh: window.innerHeight,
          });
        } catch (_e) { /* binding gone during teardown */ }
      };
      addEventListener("mousemove", function (e) {
        var now = Date.now();
        if (now - last < ${POINTER_THROTTLE_MS}) return;
        last = now;
        send("move", e);
      }, { capture: true, passive: true });
      addEventListener("mousedown", function (e) { send("down", e); }, { capture: true, passive: true });
    })();`);
  } catch (err) {
    console.error(`[peckboard-sidecar] pointer install failed: ${err}`);
  }
}
/** Answer our events route; return false for everything else. */
function handleEvents(req, res) {
  const u = new URL(req.url, "http://sidecar");
  const m = u.pathname.match(/^\/api\/pages\/([^/]+)\/events$/);
  if (!m || req.method !== "GET") return false;
  const b = buffers.get(decodeURIComponent(m[1]));
  const since = parseInt(u.searchParams.get("since") || "0", 10) || 0;
  const out = b
    ? {
        events: b.events.filter((e) => e.seq > since),
        next: b.seq,
        dropped: b.dropped,
      }
    : { events: [], next: 0, dropped: 0 };
  res.writeHead(200, { "content-type": "application/json" });
  res.end(JSON.stringify(out));
  return true;
}

/** Locate the npx-provisioned package dir via the PATH entries npx added. */
function findPackageDir() {
  for (const entry of (process.env.PATH || "").split(delimiter)) {
    if (!entry.endsWith(join("node_modules", ".bin"))) continue;
    const candidate = join(entry, "..", PKG);
    if (existsSync(join(candidate, "dist", "server", "playwright-server.js"))) {
      return candidate;
    }
  }
  return null;
}

/** Plain upstream server as a child — capture off, browsing alive. */
function fallbackToUpstream(reason) {
  console.error(
    `[peckboard-sidecar] capture unavailable (${reason}); running plain ${PKG}`,
  );
  const child = spawn(
    PKG,
    ["server", "--headless", "--no-user-profile", "--port", String(PORT)],
    { stdio: "inherit" },
  );
  child.on("error", (err) => {
    console.error(`[peckboard-sidecar] fallback spawn failed: ${err}`);
    process.exit(1);
  });
  child.on("exit", (code) => process.exit(code || 0));
  // Never orphan the fallback: it owns the port, and an orphaned instance
  // makes every future capture sidecar die EADDRINUSE while still looking
  // healthy to core's poll.
  const reap = () => {
    try {
      child.kill("SIGKILL");
    } catch {
      /* already gone */
    }
  };
  process.on("exit", reap);
  for (const sig of ["SIGTERM", "SIGINT", "SIGHUP"]) {
    process.on(sig, () => {
      reap();
      process.exit(0);
    });
  }
}

const pkgDir = findPackageDir();
if (!pkgDir) {
  fallbackToUpstream("package not found on PATH");
} else {
  try {
    const mod = await import(
      pathToFileURL(join(pkgDir, "dist", "server", "playwright-server.js")).href
    );
    const server = new mod.PlaywrightServer(PORT);

    // Intercept page registration (createPage does pages.set(id, info)
    // BEFORE the initial goto — so even first-navigation requests are
    // captured). Patching the map instead of createPage keeps us immune to
    // its return-shape details.
    const origSet = server.pages.set.bind(server.pages);
    server.pages.set = (id, info) => {
      try {
        if (info && info.page) {
          pageIds.set(info.page, id);
          attachCapture(id, info.page);
        }
      } catch (err) {
        console.error(`[peckboard-sidecar] attach failed for ${id}: ${err}`);
      }
      return origSet(id, info);
    };
    // Pointer capture must be installed BETWEEN newPage and the initial
    // goto, and AWAITED — see installPointer. createPage calls
    // browserContext.newPage, so wrap it once per (re)launched context.
    const origEnsure = server.ensureBrowser.bind(server);
    server.ensureBrowser = async (...args) => {
      const out = await origEnsure(...args);
      try {
        const ctx = server.browserContext;
        if (ctx && !ctx.__peckboardPointerPatched) {
          ctx.__peckboardPointerPatched = true;
          const origNewPage = ctx.newPage.bind(ctx);
          ctx.newPage = async (...pa) => {
            const page = await origNewPage(...pa);
            await installPointer(page);
            return page;
          };
        }
      } catch (err) {
        console.error(`[peckboard-sidecar] pointer patch failed: ${err}`);
      }
      return out;
    };
    // Upstream caches a dead browser forever: ensureBrowser only checks
    // `persistentContext == null`, so once Chrome crashes (or is killed)
    // every later page creation fails with "has been closed" until the
    // whole server restarts. Self-heal instead: reset and relaunch once
    // within the same call.
    const origCreate = server.createPage.bind(server);
    server.createPage = async (...args) => {
      try {
        return await origCreate(...args);
      } catch (err) {
        if (!String(err).includes("has been closed")) throw err;
        console.error("[peckboard-sidecar] browser context dead; relaunching");
        try {
          await server.persistentContext?.close?.();
        } catch {
          /* already gone */
        }
        server.persistentContext = null;
        server.browserContext = null;
        return await origCreate(...args);
      }
    };

    // Compose our route in front of the upstream express app (an express
    // app is itself a (req, res) handler) instead of calling
    // server.start() — immune to upstream middleware ordering.
    const srv = createServer((req, res) => {
      // Ownership marker: core health-checks this header to distinguish a
      // live capture sidecar from a foreign/orphaned server on the port.
      res.setHeader("x-peckboard-capture", "1");
      try {
        if (handleEvents(req, res)) return;
      } catch (err) {
        res.writeHead(500, { "content-type": "application/json" });
        res.end(JSON.stringify({ error: String(err) }));
        return;
      }
      server.app(req, res);
    });
    srv.on("error", (err) => {
      // Typically EADDRINUSE: an orphaned server owns the port. Exit fast
      // and loud — core reads stderr and hops to the next port.
      console.error(`[peckboard-sidecar] listen failed: ${err}`);
      process.exit(1);
    });
    srv.listen(PORT, () => {
      console.log(`[peckboard-sidecar] capturing on http://127.0.0.1:${PORT}`);
    });
  } catch (err) {
    fallbackToUpstream(String(err));
  }
}
