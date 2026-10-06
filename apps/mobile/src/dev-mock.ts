// Dev-only: `npm run dev`, then open /mock.html in any browser to iterate on
// the shell UI without Tauri. Never part of the production bundle (only
// index.html is a build input). URL hash flags (combine with commas):
//   #pair       start on the deep-link confirm screen
//   #locked     start with app lock on (code 1234) and locked
//   #nobio      device offers no biometrics
//   #bio        biometric unlock is turned on
//   #biohang    the biometric prompt never resolves
//   #nolen      status omits the code length (recovered config)
//   #storageerr first lock_unlock fails with the secure-storage error
// Alt+L locks the app (stands in for auto-lock after backgrounding).

import { emit } from "@tauri-apps/api/event";
import { mockIPC } from "@tauri-apps/api/mocks";

const boxes = [
  {
    id: "a1",
    name: "Home",
    relay: "relay.peckboard.com",
    port: 41000,
    fingerprint: "00",
    addedAt: Date.now() - 86400000 * 3,
    lastConnectedAt: Date.now() - 3600000 * 2,
    micAllowed: null,
    auth: "enrolled",
    boxFp: "K7QM-3RT2-9XZA-PL4W",
    linkExpiresAt: null,
    status: null,
  },
  {
    id: "b2",
    name: "Office",
    relay: "relay.peckboard.com",
    port: 41001,
    fingerprint: "01",
    addedAt: Date.now() - 86400000,
    lastConnectedAt: null,
    micAllowed: null,
    auth: "legacy",
    boxFp: null,
    linkExpiresAt: null,
    status: null,
  },
];

const flags = new Set(location.hash.slice(1).split(","));

let prompt: Record<string, unknown> | null = flags.has("pair")
  ? {
      id: "p1",
      link: "https://peckboard.com/pair#v=2&s=x&k=y&e=1",
      relay: "relay.peckboard.com",
      error: null,
      boxFingerprint: "K7QM-3RT2-9XZA-PL4W",
      expiresAt: Math.floor(Date.now() / 1000) + 3500,
      legacy: false,
    }
  : null;

// ---- app lock (in-memory stand-in for lock.rs) ----

type LockMethod = "code" | "pattern";
const lockState = {
  enabled: false,
  locked: false,
  method: null as LockMethod | null,
  secret: null as string | null,
  biometrics: flags.has("bio"),
  autoLock: "1m",
  failures: 0,
  retryUntil: 0,
};
const biometricKind = flags.has("nobio") ? "none" : "faceId";
let storageErrPending = flags.has("storageerr");
if (flags.has("locked")) {
  Object.assign(lockState, {
    enabled: true,
    locked: true,
    method: "code",
    secret: "1234",
  });
}

function lockStatus() {
  const left = lockState.retryUntil - Date.now();
  return {
    enabled: lockState.enabled,
    locked: lockState.locked,
    method: lockState.method,
    codeLength:
      lockState.method === "code" && !flags.has("nolen")
        ? (lockState.secret?.length ?? null)
        : null,
    biometrics: lockState.biometrics,
    biometricKind,
    autoLock: lockState.autoLock,
    retryAfterMs: left > 0 ? left : null,
    failures: lockState.failures,
  };
}

function emitLock() {
  void emit("lock-status", lockStatus());
}

function validateSecret(method: LockMethod, secret: string) {
  if (method === "code") {
    if (!/^\d{4,8}$/.test(secret)) throw "code must be 4–8 digits";
    return;
  }
  const dots = secret.split("-");
  if (dots.length < 4 || new Set(dots).size !== dots.length)
    throw "pattern needs at least 4 distinct dots";
}

/** A wrong attempt: 1–4 free, then 30 s, 1 min, 5 min, 15 min. */
function failAttempt() {
  lockState.failures += 1;
  const f = lockState.failures;
  const secs = f >= 8 ? 900 : f === 7 ? 300 : f === 6 ? 60 : f === 5 ? 30 : 0;
  if (secs) lockState.retryUntil = Date.now() + secs * 1000;
}

/** `current` as in the contract: the code/pattern, or null = biometrics. */
function checkCurrent(current: unknown) {
  const wrong = lockState.method === "code" ? "wrong code" : "wrong pattern";
  if (current === null || current === undefined) {
    if (!lockState.biometrics) throw "biometrics are not enabled";
    return; // the fake prompt always succeeds
  }
  if (lockState.retryUntil > Date.now()) throw wrong;
  if (current !== lockState.secret) {
    failAttempt();
    emitLock();
    throw wrong;
  }
  lockState.failures = 0;
}

const delay = (ms: number) => new Promise((r) => setTimeout(r, ms));

function lockNow() {
  if (!lockState.enabled) return;
  lockState.locked = true;
  emitLock();
}

document.addEventListener("keydown", (ev) => {
  if (ev.altKey && ev.key.toLowerCase() === "l") lockNow();
});

mockIPC(
  async (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    if (lockState.locked && !cmd.startsWith("lock_")) throw "locked";
    switch (cmd) {
      case "lock_status":
        return lockStatus();
      case "lock_setup": {
        if (lockState.enabled) throw "app lock is already set up";
        validateSecret(a.method as LockMethod, a.secret as string);
        Object.assign(lockState, {
          enabled: true,
          locked: false,
          method: a.method,
          secret: a.secret,
          autoLock: a.autoLock,
          failures: 0,
          retryUntil: 0,
        });
        emitLock();
        return lockStatus();
      }
      case "lock_unlock": {
        if (storageErrPending) {
          storageErrPending = false;
          throw "Couldn't read the app lock from secure storage. Try again.";
        }
        if (!lockState.locked) return { ok: true, status: lockStatus() };
        if (lockState.retryUntil > Date.now())
          return { ok: false, status: lockStatus() };
        if (a.secret !== lockState.secret) {
          failAttempt();
          emitLock();
          return { ok: false, status: lockStatus() };
        }
        lockState.failures = 0;
        lockState.retryUntil = 0;
        lockState.locked = false;
        emitLock();
        return { ok: true, status: lockStatus() };
      }
      case "lock_unlock_biometric": {
        if (!lockState.biometrics) throw "biometrics are not enabled";
        if (flags.has("biohang")) await new Promise(() => {});
        await delay(400);
        lockState.locked = false;
        lockState.failures = 0;
        lockState.retryUntil = 0;
        emitLock();
        return { ok: true, status: lockStatus() };
      }
      case "lock_change": {
        checkCurrent(a.current);
        validateSecret(a.method as LockMethod, a.secret as string);
        lockState.method = a.method as LockMethod;
        lockState.secret = a.secret as string;
        emitLock();
        return lockStatus();
      }
      case "lock_set_options": {
        checkCurrent(a.current);
        if (typeof a.biometrics === "boolean") {
          if (a.biometrics && biometricKind === "none")
            throw "biometrics are not available on this device";
          lockState.biometrics = a.biometrics;
        }
        if (typeof a.autoLock === "string") lockState.autoLock = a.autoLock;
        emitLock();
        return lockStatus();
      }
      case "lock_disable": {
        checkCurrent(a.current);
        Object.assign(lockState, {
          enabled: false,
          locked: false,
          method: null,
          secret: null,
          biometrics: false,
          failures: 0,
          retryUntil: 0,
        });
        emitLock();
        return lockStatus();
      }
      case "lock_now":
        lockNow();
        return lockStatus();
      case "list_boxes":
        return boxes;
      case "add_box": {
        const link = String(a.link ?? "");
        if (
          !link.includes("peckboard://pair/") &&
          !link.includes("https://peckboard.com/pair")
        ) {
          throw "That isn't a PeckBoard pairing link (it should start with https://peckboard.com/pair or peckboard://pair/).";
        }
        return boxes[0];
      }
      case "take_pair_link": {
        const p = prompt;
        prompt = null;
        return p;
      }
      case "confirm_pair":
        return { ...boxes[0], name: a.name || "PeckBoard" };
      case "dismiss_pair":
        return null;
      case "connect_box": {
        const st = {
          boxId: a.id,
          state: "connecting",
          message: null,
          rttMs: null,
          retryInSecs: null,
          port: 41000,
          url: "about:blank",
          everConnected: false,
          relayed: false,
        };
        // Office demonstrates the hard-NAT failure.
        if (a.id === "b2") {
          setTimeout(
            () =>
              void emit("tunnel-status", {
                ...st,
                state: "hardNat",
                message:
                  "Couldn't reach your PeckBoard from this network right now. Retrying…",
                retryInSecs: 4,
              }),
            600,
          );
        }
        return st;
      }
      default:
        return null;
    }
  },
  { shouldMockEvents: true },
);

await import("./main");
