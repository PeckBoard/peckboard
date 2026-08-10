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

// --- sessions (permissions: models_read / session_write / session_dispatch /
// session_read) — the AI-session install flow (see installSession.ts) -------

/** One selectable model from `peckboard_list_models`. The host filters to
 * thinking-capable models server-side; `id` is the account-qualified id
 * (`provider:model[@account]`) sessions are created with. */
export interface ModelChoice {
  id: string;
  display_name: string;
  provider: string;
  account_id: string | null;
  thinking: boolean;
  tier: number;
}

export function listModels(): ModelChoice[] {
  const result = hostCall("peckboard_list_models", {});
  return result?.models ?? [];
}

export interface CreateSessionInput {
  name: string;
  model: string;
  is_temp: boolean;
  folder_path?: string;
  folder_name?: string;
}

/** Returns the created session's id. */
export function createSession(input: CreateSessionInput): string {
  const result = hostCall("peckboard_create_session", input);
  const id = result?.session?.id;
  if (typeof id !== "string" || !id) {
    throw new Error("session creation returned no session id");
  }
  return id;
}

export function dispatchCapture(sessionId: string, prompt: string): void {
  hostCall("peckboard_dispatch_capture", { session_id: sessionId, prompt });
}

export interface SessionEventBrief {
  seq: number;
  kind: string;
  name: string | null;
}

/** Slim event tail — `{seq, kind, name}` only, never payloads. */
export function sessionEvents(
  sessionId: string,
  afterSeq: number,
  limit?: number,
): { events: SessionEventBrief[]; latest_seq: number | null } {
  const input: any = { session_id: sessionId, after_seq: afterSeq };
  if (typeof limit === "number") input.limit = limit;
  const result = hostCall("peckboard_session_events", input);
  return {
    events: result?.events ?? [],
    latest_seq: result?.latest_seq ?? null,
  };
}

/** Whether a session row still exists (a temp session vanishes when its
 * tab is closed) — via the ungated-by-scope brief listing. */
export function sessionExists(sessionId: string): boolean {
  const result = hostCall("peckboard_list_sessions_brief", {});
  const sessions: any[] = result?.sessions ?? [];
  return sessions.some((s) => s && s.session_id === sessionId);
}

/** The trusted scope core resolved for this call; `authority` is true for
 * an authenticated plugin-UI request, false for an MCP tool invocation. */
export function callerScope(): {
  folder_id: string | null;
  authority: boolean;
} {
  const result = hostCall("peckboard_caller_scope", {});
  return {
    folder_id: result?.folder_id ?? null,
    authority: result?.authority === true,
  };
}
