// Target registry: the local Peckboard host plus zero or more remote SSH
// hosts, configured in THIS plugin's own data_store (plugin data stores are
// hard-namespaced per plugin — this plugin cannot see ssh-fleet's hosts).
// Only a vault key reference (key_id) is ever stored here, never key
// material — see src/host.ts's SshAuth union and Auth::KeyRef in the core
// repo (src/plugin/ssh.rs).

import { nextSeq } from "./counter";
import { SshConn } from "./host";
import { storeDelete, storeGet, storeList, storePut } from "./host";

const COLLECTION = "targets";

export type TargetKind = "local" | "remote";

export interface TargetRecord {
  id: string;
  kind: TargetKind;
  label: string;
  hostname?: string;
  port?: number;
  username?: string;
  key_id?: string;
  known_host?: string;
  created_at?: string;
  updated_at?: string;
}

export const LOCAL_TARGET: TargetRecord = {
  id: "local",
  kind: "local",
  label: "Local (this host)",
};

function mintId(): string {
  return "t" + nextSeq("target_seq");
}

/** Pure validation/construction of a remote target record. Throws a plain
 * Error with a specific, user-facing message on any invalid input. */
export function buildRecord(
  input: any,
  existing: TargetRecord | null,
  idFactory: () => string = mintId,
): TargetRecord {
  const hostname = String(input?.hostname ?? existing?.hostname ?? "").trim();
  if (!hostname) throw new Error("hostname is required");

  const username = String(input?.username ?? existing?.username ?? "").trim();
  if (!username) throw new Error("username is required");

  const keyId = String(input?.key_id ?? existing?.key_id ?? "").trim();
  if (!keyId)
    throw new Error("key_id is required (pick a key from the SSH key vault)");

  let port =
    input?.port !== undefined ? Number(input.port) : (existing?.port ?? 22);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error("port must be an integer between 1 and 65535");
  }

  const label =
    String(input?.label ?? existing?.label ?? hostname).trim() || hostname;
  const knownHost = input?.known_host ?? existing?.known_host;

  return {
    id: existing?.id ?? idFactory(),
    kind: "remote",
    label,
    hostname,
    port,
    username,
    key_id: keyId,
    known_host: knownHost,
    created_at: existing?.created_at,
    updated_at: existing?.updated_at,
  };
}

export function listTargets(): TargetRecord[] {
  const remotes = storeList(COLLECTION)
    .map((i) => i.value as TargetRecord)
    .filter((t) => t && typeof t.id === "string")
    .sort((a, b) => a.label.localeCompare(b.label));
  return [LOCAL_TARGET, ...remotes];
}

export function getTarget(id: string): TargetRecord | null {
  if (id === "local") return LOCAL_TARGET;
  return (storeGet(COLLECTION, id) as TargetRecord) ?? null;
}

/** Resolve a target by id, then label, then hostname (case-insensitive). */
export function resolveTarget(ref: unknown): TargetRecord {
  const s = String(ref ?? "").trim();
  if (!s) throw new Error("target is required");
  const all = listTargets();
  const byId = all.find((t) => t.id === s);
  if (byId) return byId;
  const lower = s.toLowerCase();
  const byLabel = all.find((t) => t.label.toLowerCase() === lower);
  if (byLabel) return byLabel;
  const byHostname = all.find(
    (t) => (t.hostname ?? "").toLowerCase() === lower,
  );
  if (byHostname) return byHostname;
  throw new Error(`unknown target '${s}'`);
}

export function putTarget(rec: TargetRecord): void {
  storePut(COLLECTION, rec.id, rec);
}

export function deleteTarget(id: string): boolean {
  if (id === "local" || !getTarget(id)) return false;
  storeDelete(COLLECTION, id);
  return true;
}

/** Build the SSH connection object for a remote target. Never persisted —
 * built fresh from the stored key_id reference on every call. */
export function toConn(
  target: TargetRecord,
  connectTimeoutSecs?: number,
): SshConn {
  if (
    target.kind !== "remote" ||
    !target.hostname ||
    !target.username ||
    !target.key_id
  ) {
    throw new Error(`target '${target.id}' is not a remote SSH target`);
  }
  const conn: SshConn = {
    host: target.hostname,
    port: target.port ?? 22,
    username: target.username,
    auth: { key_id: target.key_id },
  };
  if (target.known_host) conn.known_host = target.known_host;
  if (typeof connectTimeoutSecs === "number")
    conn.connect_timeout_secs = connectTimeoutSecs;
  return conn;
}
