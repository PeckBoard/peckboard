// Dev-only: `npm run dev`, then open /mock.html in any browser to iterate on
// the shell UI without Tauri. Never part of the production bundle (only
// index.html is a build input).

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
    status: null,
  },
];

mockIPC(
  (cmd, args) => {
    const a = args as Record<string, string>;
    switch (cmd) {
      case "list_boxes":
        return boxes;
      case "add_box":
        if (!a.link.includes("peckboard://pair/")) {
          throw "That isn't a PeckBoard pairing link (it should start with peckboard://pair/).";
        }
        return boxes[0];
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
