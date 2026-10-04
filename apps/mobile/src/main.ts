// PeckBoard mobile shell: box list, pairing (QR / paste), connect screen.
// Once a box's tunnel is up the WebView navigates to its loopback URL and
// the box's own web UI takes over; Back returns here.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Format, scan } from "@tauri-apps/plugin-barcode-scanner";

type TunnelState =
  | "connecting"
  | "connected"
  | "reconnecting"
  | "boxOffline"
  | "hardNat"
  | "error"
  | "paused"
  | "stopped";

interface TunnelStatus {
  boxId: string;
  state: TunnelState;
  message: string | null;
  rttMs: number | null;
  retryInSecs: number | null;
  port: number;
  url: string;
  everConnected: boolean;
  /** Connected through the relay (no direct path from this network). */
  relayed: boolean;
}

interface BoxView {
  id: string;
  name: string;
  relay: string;
  port: number;
  fingerprint: string;
  addedAt: number;
  lastConnectedAt: number | null;
  status: TunnelStatus | null;
}

type Screen =
  | { kind: "list" }
  | {
      kind: "add";
      link?: string;
      name?: string;
      error?: string;
      busy?: boolean;
    }
  | {
      kind: "connect";
      box: BoxView;
      status: TunnelStatus | null;
      error?: string;
    }
  | { kind: "manage"; box: BoxView; confirmRemove?: boolean; error?: string }
  | { kind: "confirmPair"; link: string; relay: string };

interface PairPrompt {
  link: string;
  relay: string | null;
  error: string | null;
}

const app = document.getElementById("app")!;
const isMobile = /Android|iPhone|iPad|iPod/i.test(navigator.userAgent);
let screen: Screen = { kind: "list" };
let boxes: BoxView[] = [];

// ---- tiny DOM helper (textContent only — never innerHTML with data) ----

type Child = Node | string | null | undefined | false;
function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Record<string, unknown> = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v === undefined || v === null || v === false) continue;
    if (k.startsWith("on") && typeof v === "function") {
      el.addEventListener(k.slice(2).toLowerCase(), v as EventListener);
    } else if (k === "class") {
      el.className = String(v);
    } else {
      el.setAttribute(k, v === true ? "" : String(v));
    }
  }
  for (const c of children) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c);
  }
  return el;
}

function errorText(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : String(e);
}

// ---- status copy --------------------------------------------------------

const STATE_LABEL: Record<TunnelState, string> = {
  connecting: "Finding your box…",
  connected: "Connected",
  reconnecting: "Reconnecting…",
  boxOffline: "Box offline",
  hardNat: "Can't connect",
  error: "Connection failed",
  paused: "Paused",
  stopped: "Disconnected",
};

function badgeClass(s: TunnelState): string {
  if (s === "connected") return "badge ok";
  if (s === "boxOffline" || s === "hardNat" || s === "error")
    return "badge bad";
  return "badge busy";
}

function lastSeen(ms: number | null): string {
  if (!ms) return "Never connected";
  const mins = Math.round((Date.now() - ms) / 60000);
  if (mins < 1) return "Connected just now";
  if (mins < 60) return `Connected ${mins} min ago`;
  const hrs = Math.round(mins / 60);
  if (hrs < 48) return `Connected ${hrs} h ago`;
  return `Connected ${new Date(ms).toLocaleDateString()}`;
}

// ---- data ---------------------------------------------------------------

async function refresh() {
  boxes = await invoke<BoxView[]>("list_boxes");
}

function go(next: Screen) {
  screen = next;
  render();
}

async function openBox(box: BoxView) {
  go({ kind: "connect", box, status: box.status });
  try {
    const st = await invoke<TunnelStatus>("connect_box", { id: box.id });
    onStatus(st);
  } catch (e) {
    if (screen.kind === "connect" && screen.box.id === box.id) {
      go({ ...screen, error: errorText(e) });
    }
  }
}

function onStatus(st: TunnelStatus) {
  const b = boxes.find((x) => x.id === st.boxId);
  if (b) b.status = st.state === "stopped" ? null : st;
  if (screen.kind === "connect" && screen.box.id === st.boxId) {
    if (st.state === "connected") {
      // Gate boot URL: sets the loopback cookie, then loads the box UI.
      // The app adds a "Boxes" button to box pages that returns here.
      window.location.href = st.url;
      return;
    }
    screen = { ...screen, status: st, error: undefined };
  }
  render();
}

async function pair(link: string, name: string) {
  if (screen.kind !== "add") return;
  go({ ...screen, link, name, busy: true, error: undefined });
  try {
    const box = await invoke<BoxView>("add_box", { link, name });
    await refresh();
    await openBox(box);
  } catch (e) {
    go({ kind: "add", link, name, error: errorText(e) });
  }
}

async function scanQr(name: string) {
  try {
    const r = await scan({ windowed: false, formats: [Format.QRCode] });
    await pair(r.content, name);
  } catch (e) {
    const msg = errorText(e);
    if (!/cancel/i.test(msg))
      go({ kind: "add", name, error: `Couldn't scan: ${msg}` });
  }
}

// ---- screens ------------------------------------------------------------

function header(title: string, back?: () => void, action?: Child): HTMLElement {
  return h(
    "header",
    { class: "top" },
    back
      ? h(
          "button",
          { class: "icon-btn", "aria-label": "Back", onClick: back },
          "‹",
        )
      : h("span", { class: "logo", "aria-hidden": "true" }),
    h("h1", {}, title),
    action || h("span", { class: "spacer" }),
  );
}

function listScreen(): HTMLElement {
  const items = boxes.map((b) => {
    const st = b.status;
    return h(
      "li",
      { class: "row" },
      h(
        "button",
        { class: "row-main", onClick: () => openBox(b) },
        h("span", { class: "row-title" }, b.name),
        h(
          "span",
          { class: "row-sub" },
          st ? STATE_LABEL[st.state] : lastSeen(b.lastConnectedAt),
        ),
      ),
      st && h("span", { class: badgeClass(st.state), "aria-hidden": "true" }),
      h(
        "button",
        {
          class: "icon-btn",
          "aria-label": `Manage ${b.name}`,
          onClick: () => go({ kind: "manage", box: b }),
        },
        "⋯",
      ),
    );
  });
  return h(
    "section",
    { class: "screen" },
    header(
      "PeckBoard",
      undefined,
      h(
        "button",
        { class: "btn small", onClick: () => go({ kind: "add" }) },
        "Add box",
      ),
    ),
    boxes.length
      ? h("ul", { class: "list" }, ...items)
      : h(
          "div",
          { class: "empty" },
          h("p", { class: "lead" }, "Reach your PeckBoard from anywhere."),
          h(
            "p",
            {},
            isMobile
              ? "On your box, open Settings → Remote Access → Add phone, then scan the pairing code here."
              : "On your box, open Settings → Remote Access → Add phone, then open or paste the pairing link here.",
          ),
          h(
            "button",
            { class: "btn primary", onClick: () => go({ kind: "add" }) },
            "Pair a box",
          ),
        ),
  );
}

function addScreen(s: Extract<Screen, { kind: "add" }>): HTMLElement {
  const name = h("input", {
    id: "box-name",
    type: "text",
    placeholder: "Home",
    maxlength: 64,
    autocomplete: "off",
    value: s.name ?? "",
  });
  const link = h("textarea", {
    id: "box-link",
    rows: 3,
    placeholder: "peckboard://pair/…",
    autocapitalize: "off",
    autocorrect: "off",
    spellcheck: "false",
  });
  link.value = s.link ?? "";
  const submit = (ev: Event) => {
    ev.preventDefault();
    void pair(link.value, name.value);
  };
  return h(
    "section",
    { class: "screen" },
    header("Pair a box", () => go({ kind: "list" })),
    h(
      "form",
      { class: "form", onSubmit: submit },
      h("label", { for: "box-name" }, "Name"),
      name,
      isMobile &&
        h(
          "button",
          {
            type: "button",
            class: "btn primary block",
            disabled: s.busy,
            onClick: () => scanQr(name.value),
          },
          "Scan pairing QR code",
        ),
      h(
        "div",
        { class: "or" },
        isMobile ? "or paste the link" : "Paste the pairing link",
      ),
      link,
      s.error && h("p", { class: "field-error", role: "alert" }, s.error),
      h(
        "button",
        {
          type: "submit",
          class: isMobile ? "btn block" : "btn primary block",
          disabled: s.busy,
        },
        s.busy ? "Pairing…" : "Pair",
      ),
      h(
        "p",
        { class: "hint" },
        "Each phone needs its own pairing link — reusing your computer’s link would disconnect it.",
      ),
    ),
  );
}

function connectScreen(s: Extract<Screen, { kind: "connect" }>): HTMLElement {
  const st = s.status;
  const failed =
    !!s.error || (st && ["boxOffline", "hardNat", "error"].includes(st.state));
  const label = s.error
    ? "Couldn’t connect"
    : st
      ? STATE_LABEL[st.state]
      : "Starting…";
  const cancel = async () => {
    await invoke("disconnect_box").catch(() => {});
    await refresh();
    go({ kind: "list" });
  };
  // Restart now instead of waiting out the backoff.
  const retry = async () => {
    await invoke("disconnect_box").catch(() => {});
    await openBox(s.box);
  };
  return h(
    "section",
    { class: "screen" },
    header(s.box.name, cancel),
    h(
      "div",
      { class: "connect" },
      failed
        ? h("div", { class: "status-icon bad" }, "!")
        : h("div", { class: "spinner" }),
      h("h2", {}, label),
      (s.error || st?.message) &&
        h("p", { class: "message" }, s.error ?? st?.message ?? ""),
      st?.retryInSecs != null &&
        h("p", { class: "hint" }, `Trying again in ${st.retryInSecs} s…`),
      h(
        "p",
        { class: "hint" },
        st?.relayed
          ? `Relayed via ${s.box.relay} · end-to-end encrypted`
          : `via ${s.box.relay}`,
      ),
      h(
        "div",
        { class: "actions" },
        failed &&
          h("button", { class: "btn primary", onClick: retry }, "Try again"),
        h("button", { class: "btn", onClick: cancel }, "Cancel"),
      ),
    ),
  );
}

function manageScreen(s: Extract<Screen, { kind: "manage" }>): HTMLElement {
  const name = h("input", {
    id: "rename",
    type: "text",
    maxlength: 64,
    value: s.box.name,
    autocomplete: "off",
  });
  const save = async (ev: Event) => {
    ev.preventDefault();
    try {
      await invoke("rename_box", { id: s.box.id, name: name.value });
      await refresh();
      go({ kind: "list" });
    } catch (e) {
      go({ ...s, error: errorText(e) });
    }
  };
  const remove = async () => {
    try {
      await invoke("remove_box", { id: s.box.id });
      await refresh();
      go({ kind: "list" });
    } catch (e) {
      go({ ...s, confirmRemove: false, error: errorText(e) });
    }
  };
  return h(
    "section",
    { class: "screen" },
    header(s.box.name, () => go({ kind: "list" })),
    h(
      "form",
      { class: "form", onSubmit: save },
      h("label", { for: "rename" }, "Name"),
      name,
      s.error && h("p", { class: "field-error", role: "alert" }, s.error),
      h("button", { type: "submit", class: "btn primary block" }, "Save"),
      h(
        "dl",
        { class: "facts" },
        h("dt", {}, "Relay"),
        h("dd", {}, s.box.relay),
        h("dt", {}, "Local port"),
        h("dd", {}, String(s.box.port)),
        h("dt", {}, "Paired"),
        h("dd", {}, new Date(s.box.addedAt).toLocaleString()),
      ),
      s.confirmRemove
        ? h(
            "div",
            {
              class: "confirm",
              role: "alertdialog",
              "aria-label": "Confirm removal",
            },
            h(
              "p",
              {},
              `Remove “${s.box.name}”? This phone forgets its pairing secret; you’ll need a new link to pair again.`,
            ),
            h(
              "div",
              { class: "actions" },
              h(
                "button",
                { type: "button", class: "btn danger", onClick: remove },
                "Remove",
              ),
              h(
                "button",
                {
                  type: "button",
                  class: "btn",
                  onClick: () => go({ ...s, confirmRemove: false }),
                },
                "Cancel",
              ),
            ),
          )
        : h(
            "button",
            {
              type: "button",
              class: "btn danger-outline block",
              onClick: () => go({ ...s, confirmRemove: true }),
            },
            "Remove box",
          ),
    ),
  );
}
function confirmPairScreen(
  s: Extract<Screen, { kind: "confirmPair" }>,
): HTMLElement {
  const name = h("input", {
    id: "box-name",
    type: "text",
    placeholder: "Home",
    maxlength: 64,
    autocomplete: "off",
  });
  const confirm = (ev: Event) => {
    ev.preventDefault();
    screen = { kind: "add", link: s.link, name: name.value };
    void pair(s.link, name.value);
  };
  return h(
    "section",
    { class: "screen" },
    header("Pair a box?", () => go({ kind: "list" })),
    h(
      "form",
      { class: "form", onSubmit: confirm },
      h(
        "p",
        { class: "lead" },
        "A pairing link opened PeckBoard. Only continue if you just created it on your own box.",
      ),
      h(
        "dl",
        { class: "facts" },
        h("dt", {}, "Relay"),
        h("dd", { id: "pair-relay" }, s.relay),
      ),
      h("label", { for: "box-name" }, "Name"),
      name,
      h("button", { type: "submit", class: "btn primary block" }, "Pair"),
      h(
        "button",
        {
          type: "button",
          class: "btn block",
          onClick: () => go({ kind: "list" }),
        },
        "Cancel",
      ),
    ),
  );
}

function render() {
  let view: HTMLElement;
  switch (screen.kind) {
    case "list":
      view = listScreen();
      break;
    case "add":
      view = addScreen(screen);
      break;
    case "connect":
      view = connectScreen(screen);
      break;
    case "manage":
      view = manageScreen(screen);
      break;
    case "confirmPair":
      view = confirmPairScreen(screen);
      break;
  }
  app.replaceChildren(view);
}

/** A deep-linked pairing link is waiting: ask before pairing (never silent). */
async function takePairLink() {
  const p = await invoke<PairPrompt | null>("take_pair_link");
  if (!p) return;
  if (p.error || !p.relay) {
    go({ kind: "add", link: p.link, error: p.error ?? undefined });
  } else {
    go({ kind: "confirmPair", link: p.link, relay: p.relay });
  }
}

/** The shell is showing again (Boxes button, Back, a deep link): any tunnel
 *  still up belongs to a box UI the user just left — stop it. */
async function leaveBox() {
  const st = await invoke<TunnelStatus | null>("tunnel_status").catch(
    () => null,
  );
  if (st && st.state !== "stopped") {
    await invoke("disconnect_box").catch(() => {});
  }
}

/** Android system Back on the shell (called by the native plugin): true if
 *  handled here, false to let the app go to the background. */
(window as unknown as { __pbmBack?: () => boolean }).__pbmBack = () => {
  if (screen.kind === "list") return false;
  if (screen.kind === "connect") void leaveBox();
  go({ kind: "list" });
  return true;
};

async function main() {
  await listen<TunnelStatus>("tunnel-status", (e) => onStatus(e.payload));
  await listen("pair-link", () => void takePairLink());
  await leaveBox();
  try {
    await refresh();
  } catch (e) {
    app.replaceChildren(h("p", { class: "field-error" }, errorText(e)));
    return;
  }
  render();
  await takePairLink();
}

// Restored from the back-forward cache (iOS edge swipe back from a box):
// main() doesn't run again, and the page may still show "connecting".
window.addEventListener("pageshow", (ev) => {
  if (!ev.persisted) return;
  void leaveBox()
    .then(refresh)
    .then(() => go({ kind: "list" }));
});

void main();
