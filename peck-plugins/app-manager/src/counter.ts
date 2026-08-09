// A tiny monotonic counter kept in the plugin document store. Used for
// stable, sortable ids (targets, jobs) without needing a clock (the WASM
// sandbox has no reliable wall clock).

import { storeGet, storePut } from "./host";

const META = "_meta";

export function nextSeq(name: string): number {
  const cur = storeGet(META, name);
  const n = (typeof cur === "number" && isFinite(cur) ? cur : 0) + 1;
  storePut(META, name, n);
  return n;
}
