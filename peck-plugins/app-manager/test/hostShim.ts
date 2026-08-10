// Shared in-memory Host/Memory shim for vitest, simulating the Extism ABI
// (offset in, offset out; JSON strings in a buffer table) — same shape as
// ssh-fleet's test/defer.test.ts installHost, factored out here since three
// test files in this plugin need it (targets/jobs/tools all touch the
// data_store, and jobs/tools also touch exec).

export type Store = Record<string, Record<string, any>>;
export type Store = Record<string, Record<string, any>>;

export interface HostMocks {
  execAny?: (input: {
    command: string;
    args: string[];
    timeout_secs?: number;
  }) => any;
  sshExec?: (input: any) => any;
  sshKeyList?: () => any;
  listModels?: () => any;
  createSession?: (input: any) => any;
  dispatchCapture?: (input: any) => any;
  sessionEvents?: (input: any) => any;
  listSessionsBrief?: () => any;
  callerScope?: () => any;
}

export function installHost(store: Store, mocks: HostMocks = {}): Store {
  const bufs = new Map<bigint, string>();
  let next = 1n;
  const put = (s: string): bigint => {
    const off = next++;
    bufs.set(off, s);
    return off;
  };
  const read = (off: bigint) => JSON.parse(bufs.get(off)!);

  const fns: Record<string, (off: bigint) => bigint> = {
    peckboard_store_get: (off) => {
      const { collection, key } = read(off);
      return put(JSON.stringify({ value: store[collection]?.[key] ?? null }));
    },
    peckboard_store_put: (off) => {
      const { collection, key, data } = read(off);
      (store[collection] ||= {})[key] = data;
      return put(JSON.stringify({ ok: true }));
    },
    peckboard_store_list: (off) => {
      const { collection } = read(off);
      const items = Object.entries(store[collection] ?? {}).map(
        ([key, value]) => ({ key, value }),
      );
      return put(JSON.stringify({ items }));
    },
    peckboard_store_delete: (off) => {
      const { collection, key } = read(off);
      if (store[collection]) delete store[collection][key];
      return put(JSON.stringify({ ok: true }));
    },
    peckboard_exec_any: (off) => {
      const input = read(off);
      const result = mocks.execAny
        ? mocks.execAny(input)
        : { error: "no execAny mock installed" };
      return put(JSON.stringify(result));
    },
    peckboard_ssh_exec: (off) => {
      const input = read(off);
      const result = mocks.sshExec
        ? mocks.sshExec(input)
        : { error: "no sshExec mock installed" };
      return put(JSON.stringify(result));
    },
    peckboard_ssh_probe: () =>
      put(JSON.stringify({ error: "no sshProbe mock installed" })),
    peckboard_ssh_key_list: () => {
      const result = mocks.sshKeyList ? mocks.sshKeyList() : { keys: [] };
      return put(JSON.stringify(result));
    },
    peckboard_list_models: () => {
      const result = mocks.listModels
        ? mocks.listModels()
        : { error: "no listModels mock installed" };
      return put(JSON.stringify(result));
    },
    peckboard_create_session: (off) => {
      const input = read(off);
      const result = mocks.createSession
        ? mocks.createSession(input)
        : { error: "no createSession mock installed" };
      return put(JSON.stringify(result));
    },
    peckboard_dispatch_capture: (off) => {
      const input = read(off);
      const result = mocks.dispatchCapture
        ? mocks.dispatchCapture(input)
        : { error: "no dispatchCapture mock installed" };
      return put(JSON.stringify(result));
    },
    peckboard_session_events: (off) => {
      const input = read(off);
      const result = mocks.sessionEvents
        ? mocks.sessionEvents(input)
        : { error: "no sessionEvents mock installed" };
      return put(JSON.stringify(result));
    },
    peckboard_list_sessions_brief: () => {
      const result = mocks.listSessionsBrief
        ? mocks.listSessionsBrief()
        : { sessions: [] };
      return put(JSON.stringify(result));
    },
    peckboard_caller_scope: () => {
      const result = mocks.callerScope
        ? mocks.callerScope()
        : {
            folder_id: null,
            project_id: null,
            session_id: null,
            authority: true,
          };
      return put(JSON.stringify(result));
    },
  };

  (globalThis as any).Host = { getFunctions: () => fns };
  (globalThis as any).Memory = {
    fromString: (s: string) => ({ offset: put(s) }),
    find: (off: bigint) => ({ readString: () => bufs.get(off)! }),
  };
  return store;
}
