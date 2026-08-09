// Host calls are kept LAZY (inside functions) so the pure modules load under
// vitest without an Extism runtime. See peckboard/src/plugin/host.rs for the
// Rust side of every function called here.

type HostFn = (offset: bigint) => bigint;

export function hostCall(name: string, input: unknown): any {
  const f = (Host.getFunctions() as Record<string, HostFn>)[name];
  const mem = Memory.fromString(JSON.stringify(input));
  const out = f(mem.offset);
  const parsed = JSON.parse(Memory.find(out).readString());
  if (parsed && parsed.error !== undefined && parsed.error !== null) {
    throw new Error(String(parsed.error));
  }
  return parsed;
}

// --- data_store (permission: data_store) ---------------------------------

export function storePut(collection: string, key: string, data: unknown): void {
  hostCall("peckboard_store_put", { collection, key, data });
}

export function storeGet(collection: string, key: string): any {
  const result = hostCall("peckboard_store_get", { collection, key });
  return result?.value ?? null;
}

export function storeList(
  collection: string,
): Array<{ key: string; value: any }> {
  const result = hostCall("peckboard_store_list", { collection });
  return result?.items ?? [];
}

export function storeDelete(collection: string, key: string): void {
  hostCall("peckboard_store_delete", { collection, key });
}

// --- exec (permission: process_exec_any) ----------------------------------

export interface ExecResult {
  exit_code: number | null;
  stdout: string;
  stderr: string;
  stdout_truncated: boolean;
  stderr_truncated: boolean;
  timed_out: boolean;
}

export function execAny(
  command: string,
  args: string[],
  timeoutSecs?: number,
): ExecResult {
  const input: any = { command, args };
  if (typeof timeoutSecs === "number") input.timeout_secs = timeoutSecs;
  return hostCall("peckboard_exec_any", input) as ExecResult;
}

// --- ssh (permission: ssh; ssh_keys for a KeyRef auth) --------------------

export type SshAuth =
  | { password: string }
  | { private_key: string; passphrase?: string }
  | { key_id: string };

export interface SshConn {
  host: string;
  port: number;
  username: string;
  auth: SshAuth;
  known_host?: string;
  connect_timeout_secs?: number;
}

export interface SshExecResult {
  ok: boolean;
  exit_code: number | null;
  stdout: string;
  stderr: string;
  stdout_truncated: boolean;
  stderr_truncated: boolean;
  timed_out: boolean;
  server_fingerprint: string;
  started_at: string;
  finished_at: string;
  duration_ms: number;
}

export interface SshProbeResult {
  ok: boolean;
  server_fingerprint: string;
  latency_ms: number;
  started_at: string;
  finished_at: string;
  duration_ms: number;
}

export function sshProbe(conn: SshConn): SshProbeResult {
  return hostCall("peckboard_ssh_probe", conn) as SshProbeResult;
}

export function sshExec(
  conn: SshConn,
  command: string,
  timeoutSecs?: number,
): SshExecResult {
  const input: any = { ...conn, command };
  if (typeof timeoutSecs === "number") input.timeout_secs = timeoutSecs;
  return hostCall("peckboard_ssh_exec", input) as SshExecResult;
}

// --- ssh key vault (permission: ssh_keys) ---------------------------------

export interface SshKeyListItem {
  id: string;
  name: string;
  key_type: string;
  fingerprint: string;
  has_passphrase: boolean;
  created_at: string;
}

export function sshKeyList(): SshKeyListItem[] {
  const result = hostCall("peckboard_ssh_key_list", {});
  return result?.keys ?? [];
}
