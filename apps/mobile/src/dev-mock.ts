// Dev-only: `npm run dev`, then open /mock.html in any browser to iterate on
// the shell UI without Tauri. Never part of the production bundle (only
// index.html is a build input). Add `#pair` to the mock URL to start on the
// deep-link confirm screen.

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

let prompt: Record<string, unknown> | null = location.hash.includes("pair")
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

mockIPC(
  (cmd, args) => {
    const a = args as Record<string, string>;
    switch (cmd) {
      case "list_boxes":
        return boxes;
      case "add_box":
        if (
          !a.link.includes("peckboard://pair/") &&
          !a.link.includes("https://peckboard.com/pair")
        ) {
          throw "That isn't a PeckBoard pairing link (it should start with https://peckboard.com/pair or peckboard://pair/).";
        }
        return boxes[0];
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
