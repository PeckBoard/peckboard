// PeckBoard mobile shell: box list, pairing (QR / paste / deep link), connect
// screen. Once a box's tunnel is up the WebView navigates to its loopback
// URL and the box's own web UI takes over; Back returns here.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Format, scan } from "@tauri-apps/plugin-barcode-scanner";

// Every app command carries the shell's per-launch token (header checked by
// `commands.rs` ShellProof). The app defines it on shell origins only, before
// any page script runs; a tunnelled box page never has it.
const SHELL_HEADER = "X-PeckBoard-Shell";
const shellToken =
  (window as unknown as { __PBM_SHELL__?: string }).__PBM_SHELL__ ?? "";
async function call<T>(
  cmd: string,
  args?: Record<string, unknown>,
): Promise<T> {
  try {
    return await invoke<T>(cmd, args, {
      headers: { [SHELL_HEADER]: shellToken },
    });
  } catch (e) {
    // The core refused because the app lock engaged (e.g. auto-lock fired
    // between two shell calls): pull the status so the lock screen shows.
    if (errorText(e) === "locked" && cmd !== "lock_status" && !lock?.locked) {
      void invoke<LockStatus>("lock_status", undefined, {
        headers: { [SHELL_HEADER]: shellToken },
      })
        .then(applyLockStatus)
        .catch(() => {});
    }
    throw e;
  }
}

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

/** How a box authenticates this device (`store.rs` Auth). */
type Auth = "legacy" | "enrolling" | "enrolled";

interface BoxView {
  id: string;
  name: string;
  relay: string;
  port: number;
  fingerprint: string;
  addedAt: number;
  lastConnectedAt: number | null;
  /** Microphone answer for this box's UI; null until asked. */
  micAllowed: boolean | null;
  auth: Auth;
  /** Box key fingerprint, XXXX-XXXX-XXXX-XXXX (pairing v2). */
  boxFp: string | null;
  linkExpiresAt: number | null;
  status: TunnelStatus | null;
}

/** A deep-linked pairing link held by the app under `id` (`link.rs`). */
interface PairPrompt {
  id: string;
  link: string;
  relay: string | null;
  error: string | null;
  boxFingerprint: string | null;
  /** Advisory expiry, unix seconds; the box enforces it. */
  expiresAt: number | null;
  /** A v1 link from an older box: no fingerprint to compare. */
  legacy: boolean;
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
  | { kind: "confirmPair"; prompt: PairPrompt; busy?: boolean }
  | {
      kind: "lockSettings";
      flow?: LockFlow;
      /** Digits typed so far on a settings-screen PIN pad. */
      entry?: string;
      error?: string;
      busy?: boolean;
      shake?: boolean;
      confirmOff?: boolean;
    };

const app = document.getElementById("app")!;
const isMobile = /Android|iPhone|iPad|iPod/i.test(navigator.userAgent);
let screen: Screen = { kind: "list" };
let boxes: BoxView[] = [];
let toastText: string | null = null;
let toastTimer: number | undefined;
/** Last `lock_status`; null until the core answered (or an older core
 *  without a lock). While `lock.locked` the lock screen replaces `screen`. */
let lock: LockStatus | null = null;
/** Lock-screen entry state (not part of `screen`: the lock sits over it).
 *  `busy` = a code/pattern unlock is in flight (pad disabled). `bioBusy` = a
 *  biometric prompt is up: only its button is disabled, never the pad — a
 *  prompt that never comes back must not strand the user. */
const lockUi = {
  entry: "",
  error: "",
  shake: false,
  busy: false,
  bioBusy: false,
};
/** Serial of the latest biometric prompt: a late answer from an older
 *  (timed-out) prompt must not touch the button state. */
let bioSerial = 0;
/** Backoff deadline (ms epoch) while attempts are refused; null otherwise. */
let retryUntil: number | null = null;
let retryTimer: number | undefined;
/** A pattern drag is in progress: renders wait until it ends, so the SVG
 *  under the pointer isn't replaced mid-stroke (a toast expiring, a tunnel
 *  event, a lock-status event). */
let patternDragging = false;
let renderQueued = false;
/** The PIN pad on screen, for physical-keyboard input on desktop. */
let padKeys: {
  digit: (d: string) => void;
  del: () => void;
  submit: () => void;
} | null = null;

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

/** A short, non-blocking notice at the bottom of the screen. */
function toast(text: string) {
  toastText = text;
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => {
    toastText = null;
    render();
  }, 5000);
  render();
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

function clock(unixSecs: number): string {
  return new Date(unixSecs * 1000).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** The box list row's subtitle. */
function rowSubtitle(b: BoxView): string {
  if (b.status) return STATE_LABEL[b.status.state];
  if (b.auth === "enrolling") return "Finishing pairing… tap to retry";
  return lastSeen(b.lastConnectedAt);
}

// ---- data ---------------------------------------------------------------

async function refresh() {
  boxes = await call<BoxView[]>("list_boxes");
}

function go(next: Screen) {
  screen = next;
  render();
}

async function openBox(box: BoxView) {
  setResumeBox(box.id);
  go({ kind: "connect", box, status: box.status });
  try {
    const st = await call<TunnelStatus>("connect_box", { id: box.id });
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

/** Manual pairing (paste / scan): the app enrolls (a v2 link) and the box
 *  opens right away. */
async function pair(link: string, name: string) {
  if (screen.kind !== "add") return;
  go({ ...screen, link, name, busy: true, error: undefined });
  try {
    const box = await call<BoxView>("add_box", { link, name });
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

/** Deep-linked pairing: pairs the link the app holds for the prompt (never
 *  a string from this page), then returns to the list — the box UI is not
 *  opened on its own. */
async function confirmPair(prompt: PairPrompt, name: string) {
  if (screen.kind !== "confirmPair") return;
  go({ kind: "confirmPair", prompt, busy: true });
  try {
    const box = await call<BoxView>("confirm_pair", { id: prompt.id, name });
    await refresh();
    go({ kind: "list" });
    toast(`Paired “${box.name}”`);
  } catch (e) {
    // The prompt is released either way; the link stays editable.
    go({ kind: "add", link: prompt.link, name, error: errorText(e) });
  }
}

async function dismissPair(prompt: PairPrompt) {
  await call("dismiss_pair", { id: prompt.id }).catch(() => {});
  go({ kind: "list" });
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
        h("span", { class: "row-sub" }, rowSubtitle(b)),
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
        "div",
        { class: "top-actions" },
        lock &&
          h(
            "button",
            {
              class: "icon-btn",
              "aria-label": "App lock",
              onClick: () => go({ kind: "lockSettings" }),
            },
            lockIcon(),
          ),
        h(
          "button",
          { class: "btn small", onClick: () => go({ kind: "add" }) },
          "Add box",
        ),
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
    placeholder: "https://peckboard.com/pair#… or peckboard://pair/…",
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
        "Each device needs its own pairing link. A link works once and expires after an hour.",
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
    setResumeBox(null);
    await call("disconnect_box").catch(() => {});
    await refresh();
    go({ kind: "list" });
  };
  // Restart now instead of waiting out the backoff.
  const retry = async () => {
    await call("disconnect_box").catch(() => {});
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

function pairingLabel(b: BoxView): string {
  if (b.auth === "enrolled") return "This device's own key, pinned to the box";
  if (b.auth === "enrolling") return "Not finished — open the box to retry";
  return "Older link — secured once your box and app are both updated";
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
      await call("rename_box", { id: s.box.id, name: name.value });
      await refresh();
      go({ kind: "list" });
    } catch (e) {
      go({ ...s, error: errorText(e) });
    }
  };
  const remove = async () => {
    try {
      await call("remove_box", { id: s.box.id });
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
        h("dt", {}, "Pairing"),
        h("dd", {}, pairingLabel(s.box)),
        s.box.boxFp && h("dt", {}, "Box key"),
        s.box.boxFp && h("dd", { class: "mono" }, s.box.boxFp),
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
              `Remove “${s.box.name}”? This device forgets its pairing key; you’ll need a new link to pair again.`,
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
  const p = s.prompt;
  const name = h("input", {
    id: "box-name",
    type: "text",
    placeholder: "Home",
    maxlength: 64,
    autocomplete: "off",
  });
  const confirm = (ev: Event) => {
    ev.preventDefault();
    void confirmPair(p, name.value);
  };
  const cancel = () => void dismissPair(p);
  const now = Date.now() / 1000;
  const expired = p.expiresAt != null && now > p.expiresAt;
  return h(
    "section",
    { class: "screen" },
    header("Pair a box?", cancel),
    h(
      "form",
      { class: "form", onSubmit: confirm },
      h(
        "p",
        { class: "lead" },
        "A pairing link opened PeckBoard. Only continue if you just created it on your own box.",
      ),
      p.boxFingerprint
        ? h(
            "div",
            { class: "fingerprint-block" },
            h("div", { class: "fingerprint-label" }, "Box fingerprint"),
            h(
              "div",
              { class: "fingerprint", id: "pair-fingerprint" },
              p.boxFingerprint,
            ),
            h(
              "p",
              { class: "hint" },
              "Check this matches the code shown on your PeckBoard, next to the QR code. If it differs, cancel — the link may have been swapped.",
            ),
          )
        : h(
            "p",
            { class: "note" },
            "Older box: this link has no fingerprint to compare. The pairing is secured once your box updates.",
          ),
      h(
        "dl",
        { class: "facts" },
        h("dt", {}, "Relay"),
        h("dd", { id: "pair-relay" }, p.relay ?? ""),
        p.expiresAt != null && h("dt", {}, "Link expires"),
        p.expiresAt != null &&
          h(
            "dd",
            { class: expired ? "warn" : undefined },
            expired
              ? `${clock(p.expiresAt)} — that time has passed; your box decides. Create a new link if pairing fails.`
              : clock(p.expiresAt),
          ),
      ),
      h("label", { for: "box-name" }, "Name"),
      name,
      h(
        "button",
        { type: "submit", class: "btn primary block", disabled: s.busy },
        s.busy ? "Pairing…" : "Pair",
      ),
      h(
        "button",
        {
          type: "button",
          class: "btn block",
          disabled: s.busy,
          onClick: cancel,
        },
        "Cancel",
      ),
      s.busy &&
        h(
          "p",
          { class: "hint" },
          "Reaching your box to exchange keys. The link is used up once this succeeds.",
        ),
    ),
  );
}

// ---- app lock -----------------------------------------------------------
// Contract: tmp-scratch/app-lock-contract.md (commands, payloads, semantics).

type LockMethod = "code" | "pattern";
type AutoLock = "immediate" | "1m" | "5m" | "15m";
type BiometricKind =
  | "none"
  | "faceId"
  | "touchId"
  | "opticId"
  | "biometric"
  | "hello";

interface LockStatus {
  enabled: boolean;
  locked: boolean;
  method: LockMethod | null;
  /** Digits in the code (PIN dots + auto-submit); null for pattern / off. */
  codeLength: number | null;
  biometrics: boolean;
  /** What the device can use right now. */
  biometricKind: BiometricKind;
  autoLock: AutoLock;
  /** >0 = in backoff; attempts are refused until then. */
  retryAfterMs: number | null;
  failures: number;
}

interface UnlockResult {
  ok: boolean;
  status: LockStatus;
}

/** What the settings screen does once the current code/pattern checks out. */
type LockAction =
  | { kind: "biometrics"; on: boolean }
  | { kind: "autoLock"; value: AutoLock }
  | { kind: "change" }
  | { kind: "disable" };

type LockFlow =
  /** Set up (current null) or change (current = verified code/pattern). */
  | {
      step: "set";
      current: string | null;
      method: LockMethod;
      autoLock: AutoLock;
      stage: "choose" | "enter" | "confirm";
      first?: string;
    }
  | { step: "verify"; action: LockAction };

type LockSettings = Extract<Screen, { kind: "lockSettings" }>;
type SetFlow = Extract<LockFlow, { step: "set" }>;

const MIN_CODE = 4;
const MAX_CODE = 8;
const MIN_DOTS = 4;
/** A biometric prompt still pending after this long re-enables its button. */
const BIO_TIMEOUT_MS = 30_000;
const RESUME_KEY = "pbm.resumeBox";

const AUTO_LOCK_LABEL: Record<AutoLock, string> = {
  immediate: "Immediately",
  "1m": "After 1 minute",
  "5m": "After 5 minutes",
  "15m": "After 15 minutes",
};

const BIO_LABEL: Record<BiometricKind, string> = {
  none: "Biometrics",
  faceId: "Face ID",
  touchId: "Touch ID",
  opticId: "Optic ID",
  biometric: "Fingerprint or face",
  hello: "Windows Hello",
};

/** The box to reopen after an unlock (the core locked while it was open). */
function setResumeBox(id: string | null) {
  try {
    if (id) sessionStorage.setItem(RESUME_KEY, id);
    else sessionStorage.removeItem(RESUME_KEY);
  } catch {
    // Storage unavailable: just don't resume.
  }
}

function resumeBoxId(): string | null {
  try {
    return sessionStorage.getItem(RESUME_KEY);
  } catch {
    return null;
  }
}

/** Client-side shape check; the core validates again. */
function secretProblem(method: LockMethod, v: string): string | null {
  if (method === "code") {
    return /^\d{4,8}$/.test(v) ? null : "Use 4 to 8 digits.";
  }
  const dots = v ? v.split("-") : [];
  return dots.length < MIN_DOTS ? "Connect at least 4 dots." : null;
}

function countdown(ms: number): string {
  const s = Math.ceil(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

function retryLeftMs(): number {
  return retryUntil ? Math.max(0, retryUntil - Date.now()) : 0;
}

/** Track the backoff deadline and tick the countdown until it passes. */
function setRetry(ms: number | null) {
  window.clearInterval(retryTimer);
  retryTimer = undefined;
  if (ms && ms > 0) {
    retryUntil = Date.now() + ms;
    retryTimer = window.setInterval(() => {
      if (retryLeftMs() > 0) {
        render();
        return;
      }
      setRetry(null);
      void refreshLock().then(render);
    }, 500);
  } else {
    retryUntil = null;
  }
}

async function refreshLock() {
  try {
    const st = await call<LockStatus>("lock_status");
    lock = st;
    setRetry(st.retryAfterMs);
  } catch {
    // Keep what we have.
  }
}
/** While the core is still reading the lock back (see `lockScreen`), ask
 * again now and then in case its event was missed or the read failed. */
let checkTimer: number | null = null;
function recheckLockSoon() {
  if (checkTimer !== null) return;
  checkTimer = window.setTimeout(() => {
    checkTimer = null;
    call<LockStatus>("lock_status")
      .then(applyLockStatus)
      .catch(() => {});
  }, 1500);
}

/** New status from the core (event, command result, or boot). */
function applyLockStatus(st: LockStatus) {
  const was = lock?.locked ?? null;
  lock = st;
  setRetry(st.retryAfterMs);
  if (st.locked && was !== true) {
    lockUi.entry = "";
    lockUi.error = "";
    lockUi.busy = false;
    lockUi.bioBusy = false;
    lockUi.shake = false;
    render();
    // Offer biometrics right away, once per lock.
    if (st.biometrics && st.biometricKind !== "none") void bioUnlock();
    return;
  }
  if (!st.locked && was === true) {
    void onUnlocked();
    return;
  }
  // Boot, unlocked: main() renders once the box list is loaded.
  if (was === null) return;
  render();
}

async function onUnlocked() {
  const resume = resumeBoxId();
  screen = { kind: "list" };
  await enterShell();
  if (resume) {
    const b = boxes.find((x) => x.id === resume);
    if (b) void openBox(b);
  }
}

async function tryUnlock(secret: string) {
  if (!lock || lockUi.busy) return;
  const method = lock.method ?? "code";
  const problem = secretProblem(method, secret);
  if (problem) {
    lockUi.entry = "";
    lockUi.error = problem;
    lockUi.shake = true;
    render();
    return;
  }
  lockUi.busy = true;
  lockUi.entry = secret;
  lockUi.error = "";
  render();
  try {
    const r = await call<UnlockResult>("lock_unlock", { secret });
    lockUi.busy = false;
    lockUi.entry = "";
    if (!r.ok) {
      lockUi.error = r.status.retryAfterMs
        ? ""
        : method === "code"
          ? "Wrong code. Try again."
          : "Wrong pattern. Try again.";
      lockUi.shake = true;
    }
    applyLockStatus(r.status);
  } catch (e) {
    lockUi.busy = false;
    lockUi.entry = "";
    lockUi.error = errorText(e);
    render();
  }
}

async function bioUnlock() {
  if (!lock?.locked || lockUi.bioBusy) return;
  const serial = ++bioSerial;
  lockUi.bioBusy = true;
  lockUi.error = "";
  render();
  // The native prompt may never resolve (OS quirk, app suspended mid-prompt):
  // give the button back after a while; the pad was never gated on it.
  const stale = window.setTimeout(() => {
    if (serial !== bioSerial || !lockUi.bioBusy) return;
    lockUi.bioBusy = false;
    render();
  }, BIO_TIMEOUT_MS);
  try {
    const r = await call<UnlockResult>("lock_unlock_biometric");
    if (serial === bioSerial) lockUi.bioBusy = false;
    applyLockStatus(r.status);
  } catch (e) {
    if (serial !== bioSerial) return; // a newer prompt owns the button
    lockUi.bioBusy = false;
    lockUi.error = errorText(e);
    render();
  } finally {
    window.clearTimeout(stale);
  }
}

async function lockNow() {
  try {
    applyLockStatus(await call<LockStatus>("lock_now"));
  } catch (e) {
    toast(errorText(e));
  }
}

// -- entry widgets --

interface EntryProps {
  method: LockMethod;
  /** Known code length → dots + auto-submit; null while choosing a code. */
  codeLength: number | null;
  value: string;
  disabled?: boolean;
  shake?: boolean;
  onChange: (v: string) => void;
  onSubmit: (v: string) => void;
}

function secretEntry(p: EntryProps): HTMLElement {
  return p.method === "code" ? pinPad(p) : patternGrid(p);
}

function pinPad(p: EntryProps): HTMLElement {
  const max = p.codeLength ?? MAX_CODE;
  const digit = (d: string) => {
    if (p.disabled || p.value.length >= max) return;
    const v = p.value + d;
    if (p.codeLength != null && v.length === p.codeLength) p.onSubmit(v);
    else p.onChange(v);
  };
  const del = () => {
    if (!p.disabled && p.value) p.onChange(p.value.slice(0, -1));
  };
  const submit = () => {
    if (!p.disabled && p.value.length >= MIN_CODE) p.onSubmit(p.value);
  };
  padKeys = { digit, del, submit };
  const n = p.codeLength ?? Math.max(MIN_CODE, p.value.length);
  const dots = h("div", {
    class: p.shake ? "dots shake" : "dots",
    role: "status",
    "aria-label":
      p.codeLength == null
        ? `${p.value.length} digits entered`
        : `${p.value.length} of ${p.codeLength} digits`,
  });
  for (let i = 0; i < n; i++) {
    dots.append(
      h("span", { class: i < p.value.length ? "dot filled" : "dot" }),
    );
  }
  const key = (label: string, onClick: () => void, cls = "key") =>
    h(
      "button",
      {
        type: "button",
        class: cls,
        disabled: p.disabled,
        "aria-label": label === "⌫" ? "Delete" : undefined,
        onClick,
      },
      label,
    );
  const pad = h(
    "div",
    { class: "pad" },
    ...["1", "2", "3", "4", "5", "6", "7", "8", "9"].map((d) =>
      key(d, () => digit(d)),
    ),
    h("span", { "aria-hidden": "true" }),
    key("0", () => digit("0")),
    key("⌫", del, "key ghost"),
  );
  return h("div", { class: "entry" }, dots, pad);
}

const SVG_NS = "http://www.w3.org/2000/svg";
function svgEl<K extends keyof SVGElementTagNameMap>(
  tag: K,
  attrs: Record<string, string>,
): SVGElementTagNameMap[K] {
  const el = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, v);
  return el;
}

function lockIcon(): SVGElement {
  const svg = svgEl("svg", {
    viewBox: "0 0 24 24",
    width: "22",
    height: "22",
    "aria-hidden": "true",
  });
  svg.append(
    svgEl("rect", {
      x: "5",
      y: "11",
      width: "14",
      height: "10",
      rx: "2",
      fill: "currentColor",
    }),
    svgEl("path", {
      d: "M8 11V7a4 4 0 0 1 8 0v4",
      fill: "none",
      stroke: "currentColor",
      "stroke-width": "2",
    }),
  );
  return svg;
}

/** 3×3 pattern grid. Draws with pointer events (touch and mouse); the path
 *  is "0-1-2-5-8" (row-major dot indices). Manages its own DOM during a drag
 *  so a re-render can't drop the pointer capture; reports via onSubmit. */
function patternGrid(p: EntryProps): HTMLElement {
  const cx = (i: number) => (i % 3) * 100 + 50;
  const cy = (i: number) => Math.floor(i / 3) * 100 + 50;
  const svg = svgEl("svg", {
    viewBox: "0 0 300 300",
    class:
      "pattern" + (p.shake ? " shake" : "") + (p.disabled ? " disabled" : ""),
    role: "img",
    "aria-label": "Pattern grid: drag across at least four dots",
  });
  const line = svgEl("polyline", { class: "pattern-line" });
  const tail = svgEl("line", { class: "pattern-line tail" });
  const circles: SVGCircleElement[] = [];
  svg.append(line, tail);
  for (let i = 0; i < 9; i++) {
    const c = svgEl("circle", {
      cx: String(cx(i)),
      cy: String(cy(i)),
      r: "16",
      class: "pattern-dot",
      "data-dot": String(i),
    });
    circles.push(c);
    svg.append(c);
  }
  let path: number[] = p.value ? p.value.split("-").map(Number) : [];
  const draw = (ptr?: { x: number; y: number }) => {
    circles.forEach((c, i) =>
      c.setAttribute(
        "class",
        path.includes(i) ? "pattern-dot on" : "pattern-dot",
      ),
    );
    line.setAttribute("points", path.map((i) => `${cx(i)},${cy(i)}`).join(" "));
    if (ptr && path.length) {
      const last = path[path.length - 1];
      tail.setAttribute("x1", String(cx(last)));
      tail.setAttribute("y1", String(cy(last)));
      tail.setAttribute("x2", String(ptr.x));
      tail.setAttribute("y2", String(ptr.y));
      tail.style.display = "";
    } else {
      tail.style.display = "none";
    }
  };
  draw();
  const toLocal = (ev: PointerEvent) => {
    const r = svg.getBoundingClientRect();
    return {
      x: ((ev.clientX - r.left) * 300) / r.width,
      y: ((ev.clientY - r.top) * 300) / r.height,
    };
  };
  const hit = (pt: { x: number; y: number }) => {
    for (let i = 0; i < 9; i++) {
      if (Math.hypot(pt.x - cx(i), pt.y - cy(i)) <= 36) return i;
    }
    return -1;
  };
  let dragging = false;
  svg.addEventListener("pointerdown", (ev) => {
    if (p.disabled) return;
    ev.preventDefault();
    svg.setPointerCapture(ev.pointerId);
    dragging = true;
    patternDragging = true;
    path = [];
    const pt = toLocal(ev);
    const i = hit(pt);
    if (i >= 0) path.push(i);
    draw(pt);
  });
  svg.addEventListener("pointermove", (ev) => {
    if (!dragging) return;
    const pt = toLocal(ev);
    const i = hit(pt);
    if (i >= 0 && !path.includes(i)) path.push(i);
    draw(pt);
  });
  const end = () => {
    if (!dragging) return;
    dragging = false;
    patternDragging = false;
    draw();
    if (renderQueued) {
      renderQueued = false;
      render();
    }
    if (path.length) p.onSubmit(path.join("-"));
  };
  svg.addEventListener("pointerup", end);
  svg.addEventListener("pointercancel", end);
  return h("div", { class: "entry" }, svg);
}

function methodPicker(
  value: LockMethod,
  onChange: (m: LockMethod) => void,
): HTMLElement {
  const opt = (m: LockMethod, label: string, sub: string) =>
    h(
      "button",
      {
        type: "button",
        role: "radio",
        class: m === value ? "seg on" : "seg",
        "aria-checked": String(m === value),
        onClick: () => onChange(m),
      },
      h("span", { class: "seg-title" }, label),
      h("span", { class: "seg-sub" }, sub),
    );
  return h(
    "div",
    { class: "segmented", role: "radiogroup", "aria-label": "Unlock method" },
    opt("code", "Code", "4–8 digits"),
    opt("pattern", "Pattern", "Connect 4+ dots"),
  );
}

function autoLockSelect(
  value: AutoLock,
  onChange: (v: AutoLock) => void,
): HTMLSelectElement {
  const sel = h(
    "select",
    { id: "auto-lock", "aria-label": "Lock automatically" },
    ...(Object.keys(AUTO_LOCK_LABEL) as AutoLock[]).map((v) =>
      h("option", { value: v, selected: v === value }, AUTO_LOCK_LABEL[v]),
    ),
  );
  sel.addEventListener("change", () => onChange(sel.value as AutoLock));
  return sel;
}

// -- lock screen --

function lockScreen(st: LockStatus): HTMLElement {
  // Locked before the core has read the lock back from secure storage
  // (first launch after an update): nothing to enter yet; a `lock-status`
  // event follows within a moment.
  if (!st.enabled) {
    recheckLockSoon();
    return h(
      "section",
      { class: "screen lock" },
      h(
        "div",
        { class: "lock-brand" },
        h("span", { class: "logo big", "aria-hidden": "true" }),
        h("h1", {}, "PeckBoard"),
      ),
      h("p", { class: "lock-prompt" }, "Checking app lock…"),
    );
  }
  const method = st.method ?? "code";
  const wait = retryLeftMs();
  const disabled = lockUi.busy || wait > 0;
  const bio = st.biometrics && st.biometricKind !== "none";
  return h(
    "section",
    { class: "screen lock" },
    h(
      "div",
      { class: "lock-brand" },
      h("span", { class: "logo big", "aria-hidden": "true" }),
      h("h1", {}, "PeckBoard"),
    ),
    h(
      "p",
      { class: "lock-prompt" },
      method === "code" ? "Enter your code" : "Draw your pattern",
    ),
    secretEntry({
      method,
      codeLength: st.codeLength,
      value: lockUi.entry,
      disabled,
      shake: lockUi.shake,
      onChange: (v) => {
        lockUi.entry = v;
        lockUi.error = "";
        render();
      },
      onSubmit: (v) => void tryUnlock(v),
    }),
    wait > 0
      ? h(
          "p",
          { class: "field-error", role: "alert" },
          `Too many attempts. Try again in ${countdown(wait)}.`,
        )
      : lockUi.error &&
          h("p", { class: "field-error", role: "alert" }, lockUi.error),
    // No known length (recovered config): no auto-submit, so an explicit key.
    method === "code" &&
      st.codeLength == null &&
      h(
        "button",
        {
          type: "button",
          class: "btn primary block",
          disabled: disabled || lockUi.entry.length < MIN_CODE,
          onClick: () => void tryUnlock(lockUi.entry),
        },
        "Unlock",
      ),
    bio &&
      h(
        "button",
        {
          type: "button",
          class: "btn block",
          disabled: lockUi.bioBusy,
          onClick: () => void bioUnlock(),
        },
        `Unlock with ${BIO_LABEL[st.biometricKind]}`,
      ),
  );
}

// -- settings screen --

function lockSettingsScreen(s: LockSettings): HTMLElement {
  const st = lock;
  const back = () =>
    s.flow && st?.enabled ? go({ kind: "lockSettings" }) : go({ kind: "list" });
  let body: HTMLElement;
  if (!st) {
    body = h(
      "p",
      { class: "note" },
      "App lock isn't available in this version of the app.",
    );
  } else if (s.flow?.step === "verify") {
    body = verifyFlow(s, s.flow.action, st);
  } else if (s.flow?.step === "set") {
    body = setFlow(s, s.flow);
  } else if (!st.enabled) {
    body = setFlow(s, {
      step: "set",
      current: null,
      method: "code",
      autoLock: "1m",
      stage: "choose",
    });
  } else {
    body = lockOptions(s, st);
  }
  return h("section", { class: "screen" }, header("App lock", back), body);
}

function settingRow(title: string, sub: string, control: Child): HTMLElement {
  return h(
    "li",
    { class: "setting" },
    h(
      "div",
      { class: "setting-text" },
      h("span", { class: "row-title" }, title),
      h("span", { class: "row-sub" }, sub),
    ),
    control,
  );
}

function lockOptions(s: LockSettings, st: LockStatus): HTMLElement {
  const verify = (action: LockAction) =>
    go({ kind: "lockSettings", flow: { step: "verify", action } });
  const bioAvail = st.biometricKind !== "none";
  const bioToggle = h("input", {
    type: "checkbox",
    role: "switch",
    id: "bio-toggle",
    checked: st.biometrics,
  });
  bioToggle.addEventListener("change", () =>
    verify({ kind: "biometrics", on: bioToggle.checked }),
  );
  return h(
    "div",
    { class: "form" },
    s.error && h("p", { class: "field-error", role: "alert" }, s.error),
    h(
      "ul",
      { class: "settings" },
      settingRow(
        "Unlock with",
        st.method === "code"
          ? st.codeLength
            ? `${st.codeLength}-digit code`
            : "Code"
          : "Pattern",
        h(
          "button",
          {
            type: "button",
            class: "btn small",
            onClick: () => verify({ kind: "change" }),
          },
          "Change",
        ),
      ),
      bioAvail &&
        settingRow(
          BIO_LABEL[st.biometricKind],
          st.biometrics
            ? "Unlocks the app; your code or pattern stays as fallback"
            : "Off",
          h(
            "label",
            { class: "switch", for: "bio-toggle" },
            bioToggle,
            h("span", { class: "switch-track", "aria-hidden": "true" }),
            h(
              "span",
              { class: "sr-only" },
              `Unlock with ${BIO_LABEL[st.biometricKind]}`,
            ),
          ),
        ),
      settingRow(
        "Lock automatically",
        "After the app leaves the foreground",
        autoLockSelect(st.autoLock, (value) =>
          verify({ kind: "autoLock", value }),
        ),
      ),
    ),
    !bioAvail &&
      h(
        "p",
        { class: "note" },
        "Biometric unlock isn't available: this device has no fingerprint or face enrolled.",
      ),
    h(
      "button",
      { type: "button", class: "btn block", onClick: () => void lockNow() },
      "Lock now",
    ),
    s.confirmOff
      ? h(
          "div",
          {
            class: "confirm",
            role: "alertdialog",
            "aria-label": "Confirm turning off app lock",
          },
          h(
            "p",
            {},
            "Turn off app lock? Anyone with this device can open your boxes.",
          ),
          h(
            "div",
            { class: "actions" },
            h(
              "button",
              {
                type: "button",
                class: "btn danger",
                onClick: () => verify({ kind: "disable" }),
              },
              "Turn off",
            ),
            h(
              "button",
              {
                type: "button",
                class: "btn",
                onClick: () => go({ ...s, confirmOff: false }),
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
            onClick: () => go({ ...s, confirmOff: true }),
          },
          "Turn off app lock",
        ),
  );
}

function actionReason(a: LockAction, st: LockStatus): string {
  switch (a.kind) {
    case "biometrics":
      return a.on
        ? `to turn on ${BIO_LABEL[st.biometricKind]} unlock`
        : `to turn off ${BIO_LABEL[st.biometricKind]} unlock`;
    case "autoLock":
      return "to change when the app locks";
    case "change":
      return "to choose a new code or pattern";
    case "disable":
      return "to turn off app lock";
  }
}

function verifyFlow(
  s: LockSettings,
  action: LockAction,
  st: LockStatus,
): HTMLElement {
  const method = st.method ?? "code";
  const wait = retryLeftMs();
  const disabled = s.busy || wait > 0;
  const bio =
    action.kind !== "change" && st.biometrics && st.biometricKind !== "none";
  return h(
    "div",
    { class: "form entry-form" },
    h(
      "h2",
      { class: "entry-title" },
      method === "code"
        ? "Enter your current code"
        : "Draw your current pattern",
    ),
    h("p", { class: "hint center" }, `…${actionReason(action, st)}.`),
    secretEntry({
      method,
      codeLength: st.codeLength,
      value: s.entry ?? "",
      disabled,
      shake: s.shake,
      onChange: (v) => go({ ...s, entry: v, error: undefined }),
      onSubmit: (v) => void runAction(s, action, v),
    }),
    wait > 0
      ? h(
          "p",
          { class: "field-error", role: "alert" },
          `Too many attempts. Try again in ${countdown(wait)}.`,
        )
      : s.error && h("p", { class: "field-error", role: "alert" }, s.error),
    method === "code" &&
      st.codeLength == null &&
      h(
        "button",
        {
          type: "button",
          class: "btn primary block",
          disabled: disabled || (s.entry ?? "").length < MIN_CODE,
          onClick: () => void runAction(s, action, s.entry ?? ""),
        },
        "Continue",
      ),
    bio &&
      h(
        "button",
        {
          type: "button",
          class: "btn block",
          disabled: s.busy,
          onClick: () => void runAction(s, action, null),
        },
        `Use ${BIO_LABEL[st.biometricKind]} instead`,
      ),
    h(
      "button",
      {
        type: "button",
        class: "btn block",
        disabled: s.busy,
        onClick: () => go({ kind: "lockSettings" }),
      },
      "Cancel",
    ),
  );
}

/** Run a settings action with the current code/pattern (or null = let the
 *  core re-authenticate with biometrics). */
async function runAction(
  s: LockSettings,
  action: LockAction,
  current: string | null,
) {
  if (!lock) return;
  const method = lock.method ?? "code";
  if (current != null) {
    const problem = secretProblem(method, current);
    if (problem) {
      go({ ...s, entry: "", error: problem, shake: true });
      return;
    }
  }
  go({ ...s, busy: true, entry: current ?? "", error: undefined });
  try {
    let st: LockStatus;
    switch (action.kind) {
      case "biometrics":
        st = await call<LockStatus>("lock_set_options", {
          current,
          biometrics: action.on,
        });
        break;
      case "autoLock":
        st = await call<LockStatus>("lock_set_options", {
          current,
          autoLock: action.value,
        });
        break;
      case "disable":
        st = await call<LockStatus>("lock_disable", { current });
        break;
      case "change": {
        if (current == null) return;
        // No options: just checks `current`, so a wrong one is caught here
        // rather than after the user has chosen and confirmed a new secret.
        st = await call<LockStatus>("lock_set_options", { current });
        lock = st;
        setRetry(st.retryAfterMs);
        go({
          kind: "lockSettings",
          flow: {
            step: "set",
            current,
            method: st.method ?? "code",
            autoLock: st.autoLock,
            stage: "choose",
          },
        });
        return;
      }
    }
    lock = st;
    setRetry(st.retryAfterMs);
    go({ kind: "lockSettings" });
    toast(
      action.kind === "disable"
        ? "App lock is off"
        : action.kind === "biometrics"
          ? `${BIO_LABEL[st.biometricKind]} unlock is ${action.on ? "on" : "off"}`
          : `Locks ${AUTO_LOCK_LABEL[action.value].toLowerCase()}`,
    );
  } catch (e) {
    const msg = errorText(e);
    if (msg === "wrong code" || msg === "wrong pattern") {
      await refreshLock(); // picks up a backoff deadline
      go({
        ...s,
        busy: false,
        entry: "",
        error: lock?.retryAfterMs
          ? undefined
          : msg === "wrong code"
            ? "Wrong code. Try again."
            : "Wrong pattern. Try again.",
        shake: true,
      });
    } else {
      // e.g. the biometric prompt was cancelled or unavailable.
      go({ ...s, busy: false, entry: "", error: msg });
    }
  }
}

/** Set up (f.current null) or change (f.current verified) the secret. */
function setFlow(s: LockSettings, f: SetFlow): HTMLElement {
  const upd = (patch: Partial<SetFlow>) =>
    go({
      ...s,
      flow: { ...f, ...patch },
      entry: "",
      error: undefined,
      shake: false,
    });
  const setup = f.current == null;
  if (f.stage === "choose") {
    return h(
      "form",
      {
        class: "form",
        onSubmit: (ev: Event) => {
          ev.preventDefault();
          upd({ stage: "enter" });
        },
      },
      s.error && h("p", { class: "field-error", role: "alert" }, s.error),
      setup && h("p", { class: "lead" }, "Lock PeckBoard on this device"),
      setup &&
        h(
          "p",
          { class: "message" },
          "Opening the app will need your code or pattern first. Wrong attempts only slow things down; nothing is wiped.",
        ),
      h("label", {}, setup ? "Unlock with" : "New unlock method"),
      methodPicker(f.method, (method) => upd({ method })),
      setup && h("label", { for: "auto-lock" }, "Lock automatically"),
      setup && autoLockSelect(f.autoLock, (autoLock) => upd({ autoLock })),
      h("button", { type: "submit", class: "btn primary block" }, "Continue"),
      !setup &&
        h(
          "button",
          {
            type: "button",
            class: "btn block",
            onClick: () => go({ kind: "lockSettings" }),
          },
          "Cancel",
        ),
    );
  }
  const confirm = f.stage === "confirm";
  const title =
    f.method === "code"
      ? confirm
        ? "Enter your code again"
        : "Choose a code"
      : confirm
        ? "Draw your pattern again"
        : "Choose a pattern";
  const entry = s.entry ?? "";
  const codeLength =
    f.method === "code" && confirm && f.first ? f.first.length : null;
  const onSubmit = (v: string) => void submitSecret(s, f, v);
  return h(
    "div",
    { class: "form entry-form" },
    h("h2", { class: "entry-title" }, title),
    h(
      "p",
      { class: "hint center" },
      f.method === "code" ? "4 to 8 digits." : "Connect at least 4 dots.",
    ),
    secretEntry({
      method: f.method,
      codeLength,
      value: entry,
      disabled: s.busy,
      shake: s.shake,
      onChange: (v) => go({ ...s, entry: v, error: undefined }),
      onSubmit,
    }),
    s.error && h("p", { class: "field-error", role: "alert" }, s.error),
    f.method === "code" &&
      codeLength == null &&
      h(
        "button",
        {
          type: "button",
          class: "btn primary block",
          disabled: s.busy || entry.length < MIN_CODE,
          onClick: () => onSubmit(entry),
        },
        "Continue",
      ),
    h(
      "button",
      {
        type: "button",
        class: "btn block",
        disabled: s.busy,
        onClick: () =>
          upd({ stage: confirm ? "enter" : "choose", first: undefined }),
      },
      "Back",
    ),
  );
}

async function submitSecret(s: LockSettings, f: SetFlow, v: string) {
  const problem = secretProblem(f.method, v);
  if (problem) {
    go({ ...s, entry: "", error: problem, shake: true });
    return;
  }
  if (f.stage === "enter") {
    go({
      ...s,
      flow: { ...f, stage: "confirm", first: v },
      entry: "",
      error: undefined,
      shake: false,
    });
    return;
  }
  if (v !== f.first) {
    go({
      ...s,
      flow: { ...f, stage: "enter", first: undefined },
      entry: "",
      error:
        f.method === "code"
          ? "The codes don't match. Choose your code again."
          : "The patterns don't match. Draw your pattern again.",
      shake: true,
    });
    return;
  }
  go({ ...s, busy: true, entry: v, error: undefined });
  try {
    const st =
      f.current == null
        ? await call<LockStatus>("lock_setup", {
            method: f.method,
            secret: v,
            autoLock: f.autoLock,
          })
        : await call<LockStatus>("lock_change", {
            current: f.current,
            method: f.method,
            secret: v,
          });
    lock = st;
    setRetry(st.retryAfterMs);
    go({ kind: "lockSettings" });
    toast(
      f.current == null
        ? "App lock is on"
        : f.method === "code"
          ? "Code changed"
          : "Pattern changed",
    );
  } catch (e) {
    go({ kind: "lockSettings", error: errorText(e) });
  }
}

// Physical keyboard on desktop: digits, Backspace and Enter drive the PIN pad.
window.addEventListener("keydown", (ev) => {
  if (!padKeys || ev.metaKey || ev.ctrlKey || ev.altKey) return;
  const t = ev.target as HTMLElement | null;
  const tag = t?.tagName;
  if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return;
  if (/^[0-9]$/.test(ev.key)) padKeys.digit(ev.key);
  else if (ev.key === "Backspace") padKeys.del();
  else if (ev.key === "Enter" && tag !== "BUTTON") padKeys.submit();
  else return;
  ev.preventDefault();
});

function render() {
  if (patternDragging) {
    renderQueued = true;
    return;
  }
  padKeys = null;
  let view: HTMLElement;
  if (lock?.locked) {
    view = lockScreen(lock);
  } else {
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
      case "lockSettings":
        view = lockSettingsScreen(screen);
        break;
    }
  }
  if (toastText) {
    view.append(h("div", { class: "toast", role: "status" }, toastText));
  }
  app.replaceChildren(view);
  // The shake plays once per error: clear the flag now (no re-render) so
  // the next render drops the class instead of replaying the animation.
  lockUi.shake = false;
  if (screen.kind === "lockSettings" && screen.shake) {
    screen = { ...screen, shake: false };
  }
}

/** A deep-linked pairing link is waiting: ask before pairing (never silent). */
async function takePairLink() {
  const p = await call<PairPrompt | null>("take_pair_link");
  if (!p) return;
  if (p.error || !p.relay) {
    // Invalid: release the prompt and let the user fix the text.
    await call("dismiss_pair", { id: p.id }).catch(() => {});
    go({ kind: "add", link: p.link, error: p.error ?? undefined });
  } else {
    go({ kind: "confirmPair", prompt: p });
  }
}

/** The shell is showing again (Boxes button, Back, a deep link): any tunnel
 *  still up belongs to a box UI the user just left — stop it. */
async function leaveBox() {
  setResumeBox(null);
  const st = await call<TunnelStatus | null>("tunnel_status").catch(() => null);
  if (st && st.state !== "stopped") {
    await call("disconnect_box").catch(() => {});
  }
}

/** Android system Back on the shell (called by the native plugin): true if
 *  handled here, false to let the app go to the background. */
(window as unknown as { __pbmBack?: () => boolean }).__pbmBack = () => {
  if (lock?.locked) return false;
  if (screen.kind === "list") return false;
  if (screen.kind === "connect") void leaveBox();
  if (screen.kind === "confirmPair") {
    void dismissPair(screen.prompt);
    return true;
  }
  if (screen.kind === "lockSettings" && screen.flow && lock?.enabled) {
    go({ kind: "lockSettings" });
    return true;
  }
  go({ kind: "list" });
  return true;
};

/** Unlocked (at boot or after the lock screen): load the box list and pick
 *  up a waiting pairing link. */
async function enterShell() {
  await leaveBox();
  try {
    await refresh();
  } catch (e) {
    // "locked": the lock screen is already showing (see `call`).
    if (errorText(e) === "locked") return;
    app.replaceChildren(h("p", { class: "field-error" }, errorText(e)));
    return;
  }
  render();
  await takePairLink();
}

async function main() {
  await listen<TunnelStatus>("tunnel-status", (e) => onStatus(e.payload));
  await listen("pair-link", () => void takePairLink());
  await listen("pair-link-ignored", () =>
    toast("Another pairing link was ignored while this one is open."),
  );
  await listen<LockStatus>("lock-status", (e) => applyLockStatus(e.payload));
  // The lock comes first: while locked every other command is refused.
  try {
    applyLockStatus(await call<LockStatus>("lock_status"));
  } catch {
    // An older core without a lock: nothing to gate.
  }
  if (lock?.locked) return;
  await enterShell();
}

// Restored from the back-forward cache (iOS edge swipe back from a box):
// main() doesn't run again, and the page may still show "connecting".
window.addEventListener("pageshow", (ev) => {
  if (!ev.persisted || lock?.locked) return;
  void leaveBox()
    .then(refresh)
    .then(() => go({ kind: "list" }))
    .catch(() => {});
});

void main();
