// Install/remove jobs: a catalog command is launched detached (nohup, output
// redirected to a logfile, PID captured) so app_install/app_remove return
// immediately instead of blocking for the lifetime of the plugin call —
// there is no core-side job/process-tracking primitive, so this state
// machine is entirely plugin-managed via the data_store.
//
// The pure script-building/parsing functions below need no host at all and
// are unit-tested directly; createJob/getJob/currentJobFor/pollJob touch the
// data_store and exec.ts, same split as ssh-fleet's hosts.ts.

import { nextSeq } from "./counter";
import { PackageManager } from "./distro";
import { storeGet, storePut } from "./host";
import { runOnTarget } from "./exec";
import { TargetRecord } from "./targets";

const JOBS_COLLECTION = "jobs";
const CURRENT_COLLECTION = "current_job";
const STATUS_POLL_TIMEOUT_SECS = 20;

const EXIT_SENTINEL_PREFIX = "PECKBOARD_EXIT:";
const RUNNING_SENTINEL = "PECKBOARD_RUNNING";
const EXITED_SENTINEL = "PECKBOARD_EXITED";

export type JobAction = "install" | "remove";
export type JobStatus = "running" | "succeeded" | "failed";

export interface JobRecord {
  id: string;
  target_id: string;
  app_id: string;
  action: JobAction;
  pid: number;
  logfile: string;
  status: JobStatus;
  exit_code?: number | null;
  /** Install jobs: the package manager whose snapshot bracket wraps the
   * recipe (null = no bracket) and which recipe was chosen — set at launch,
   * consumed when the job settles (see provenance.ts). */
  pm?: PackageManager | null;
  method?: PackageManager | "vendor" | "pip";
  /** How the job runs: a detached script (default for records that predate
   * the field) or a temporary AI install session (see installSession.ts).
   * Session jobs have no pid and no logfile of their own — progress is the
   * session's slim event tail, and the snapshot bracket is taken by the
   * plugin around the session's lifetime instead of inside a script. */
  kind?: "script" | "session";
  /** Session jobs: the temp session performing the install. */
  session_id?: string;
  /** Session jobs: the account-qualified model the session runs on. */
  model?: string;
  /** Session jobs: the event-log cursor consumed so far. */
  last_seq?: number;
  /** Session jobs: rendered tool-level activity lines (bounded; event kinds
   * and tool names only — event payloads are never available to plugins). */
  activity?: string[];
  /** Session jobs: total events observed so far. */
  events_total?: number;
  /** Session jobs: the session is waiting on a user answer (askpass or a
   * permission question) — surfaced so the page can say "open the session". */
  question_open?: boolean;
  /** Session jobs: one-sentence outcome note (why it failed, how it ended). */
  message?: string;
}

// --- pure: script building/parsing ----------------------------------------

export function shQuote(s: string): string {
  return `'${s.replace(/'/g, `'\\''`)}'`;
}

export function logPathFor(jobId: string): string {
  return `/tmp/peckboard-lam-${jobId}.log`;
}

/**
 * Wrap a catalog command so it runs detached, with stdout+stderr redirected
 * to `logPath` and its exit code recorded as a trailing sentinel line (see
 * parseStatusOutput). POSIX single-quote escaping makes this safe regardless
 * of what `command` or `logPath` contain — no shell injection risk even
 * though the outer script is built by string concatenation.
 */
export function buildBackgroundScript(
  command: string,
  logPath: string,
): string {
  const inner = `${command}; echo ${EXIT_SENTINEL_PREFIX}$?`;
  return `nohup sh -c ${shQuote(inner)} > ${shQuote(logPath)} 2>&1 < /dev/null & echo $!`;
}

/** The combined poll script app_status runs: is the pid still alive, plus
 * the tail of the logfile. */
export function buildStatusScript(pid: number, logPath: string): string {
  return (
    `if kill -0 ${pid} 2>/dev/null; then echo ${RUNNING_SENTINEL}; ` +
    `else echo ${EXITED_SENTINEL}; fi; tail -n 200 ${shQuote(logPath)} 2>/dev/null`
  );
}

export interface ParsedStatus {
  running: boolean;
  tail: string;
  exitCode: number | null;
}

/** Parse buildStatusScript's stdout: the running flag, the exit code (if the
 * completion sentinel is present), and the log tail with sentinel lines
 * stripped out. */
export function parseStatusOutput(raw: string): ParsedStatus {
  let running = false;
  let exitCode: number | null = null;
  const kept: string[] = [];
  for (const line of raw.split("\n")) {
    if (line === RUNNING_SENTINEL) {
      running = true;
      continue;
    }
    if (line === EXITED_SENTINEL) {
      running = false;
      continue;
    }
    if (line.startsWith(EXIT_SENTINEL_PREFIX)) {
      const n = Number(line.slice(EXIT_SENTINEL_PREFIX.length));
      exitCode = Number.isFinite(n) ? n : null;
      continue;
    }
    kept.push(line);
  }
  return { running, tail: kept.join("\n").trim(), exitCode };
}

/**
 * Fold a poll result into the job's next status. A pid that's gone but left
 * no exit sentinel is a crash (killed, OOM, host reboot) — reported as
 * failed rather than silently left "running" forever.
 */
export function deriveJobState(parsed: ParsedStatus): JobStatus {
  if (parsed.running) return "running";
  if (parsed.exitCode === 0) return "succeeded";
  return "failed";
}

// --- store-backed job records -----------------------------------------------

function mintJobId(): string {
  return "j" + nextSeq("job_seq");
}

function currentKey(targetId: string, appId: string): string {
  return `${targetId}:${appId}`;
}
export function createJob(
  targetId: string,
  appId: string,
  action: JobAction,
  meta: Pick<
    JobRecord,
    "pm" | "method" | "kind" | "session_id" | "model" | "last_seq"
  > = {},
): JobRecord {
  const id = mintJobId();
  const job: JobRecord = {
    id,
    target_id: targetId,
    app_id: appId,
    action,
    pid: 0,
    logfile: logPathFor(id),
    status: "running",
    ...meta,
  };
  storePut(JOBS_COLLECTION, job.id, job);
  storePut(CURRENT_COLLECTION, currentKey(targetId, appId), job.id);
  return job;
}

export function getJob(id: string): JobRecord | null {
  return (storeGet(JOBS_COLLECTION, id) as JobRecord) ?? null;
}

export function currentJobFor(
  targetId: string,
  appId: string,
): JobRecord | null {
  const id = storeGet(CURRENT_COLLECTION, currentKey(targetId, appId)) as
    | string
    | null;
  return id ? getJob(id) : null;
}

export function putJob(job: JobRecord): void {
  storePut(JOBS_COLLECTION, job.id, job);
}

/**
 * Poll a running job's live state on its target and persist any change.
 * Always re-tails the log (cheap) even for an already-finished job, so
 * app_status can keep showing recent output; the pid-liveness check and
 * status transition only apply while the job is still "running".
 */
export function pollJob(
  target: TargetRecord,
  job: JobRecord,
): { job: JobRecord; tail: string } {
  const res = runOnTarget(
    target,
    buildStatusScript(job.pid, job.logfile),
    STATUS_POLL_TIMEOUT_SECS,
  );
  const parsed = parseStatusOutput(res.stdout);
  if (job.status !== "running") {
    return { job, tail: parsed.tail };
  }
  const status = deriveJobState(parsed);
  if (status !== job.status) {
    const updated: JobRecord = { ...job, status, exit_code: parsed.exitCode };
    putJob(updated);
    return { job: updated, tail: parsed.tail };
  }
  return { job, tail: parsed.tail };
}
