// AI-session installs. `app_install` on the LOCAL target no longer launches
// a detached script: it creates a TEMPORARY AI session on a user-picked
// account + model (thinking-capable only — `peckboard_list_models` filters
// server-side) and dispatches an install prompt at it. Removal stays
// script-based on purpose (destructive; a scripted `apt remove` is more
// predictable than an agent), and remote targets keep the script path too —
// an AI session runs on the Peckboard host and has no path to a remote
// target's credentials.
//
// Honesty rules baked in here:
//   - Progress is the session's SLIM event tail ({seq, kind, name} — core
//     never exposes payloads to plugins), so the page shows tool-level
//     activity plus a link to the real session. It never fakes a log.
//   - Provenance still comes from the package-DB snapshot bracket taken by
//     THIS plugin around the session's lifetime (before dispatch / after the
//     run ends). The agent's own account of what it installed is never
//     trusted or parsed; success is decided by re-running the app's detect
//     probe, and the delta is the record (see provenance.ts).
//   - A session that ends without completing (tab closed, interrupted,
//     crashed) lands the job in a clear failed state — never a bogus empty
//     delta recorded as success.
//
// Pure helpers (model filtering, request/prompt building, event folding,
// outcome derivation) are exported for vitest; the two host-touching
// entry points are startSessionInstall and pollSessionJob.

import { CatalogApp, installRecipeFor } from "./catalog";
import { findAnyApp } from "./customApps";
import { PackageManager } from "./distro";
import { runOnTarget } from "./exec";
import {
  ModelChoice,
  SessionEventBrief,
  callerScope,
  createSession,
  dispatchCapture,
  listModels,
  sessionEvents,
  sessionExists,
  storeGet,
  storePut,
} from "./host";
import { JobRecord, JobStatus, createJob, putJob, shQuote } from "./jobs";
import { buildSnapshotStep, snapshotPathFor } from "./provenance";
import { TargetRecord } from "./targets";
import { errMsg } from "./verdict";

const SETTINGS_COLLECTION = "settings";
const DEFAULT_MODEL_KEY = "install_model";

/** Where install sessions live: the same `~/peckboard-installs` convention
 * as the core MCP install flow (`suggested_install_folder`), one shared
 * folder for this plugin. Core expands `~`, registers the folder row and
 * creates the directory server-side (create_session's `folder_path`). */
export const INSTALL_FOLDER_PATH = "~/peckboard-installs/app-manager";
export const INSTALL_FOLDER_NAME = "App installs";

const SNAPSHOT_TIMEOUT_SECS = 30;
const PROBE_TIMEOUT_SECS = 15;
const CLEANUP_TIMEOUT_SECS = 10;
const EVENTS_PAGE_LIMIT = 200;
/** Rolling activity window kept on the job record. */
const ACTIVITY_MAX = 12;

// --- model choices ----------------------------------------------------------

/**
 * Validate the host's model catalog into typed choices. The host already
 * filters to thinking models server-side; this is a STRICT belt-and-braces
 * check (an entry not explicitly `thinking: true` is dropped), never a loose
 * re-derivation from names or ids.
 */
export function thinkingModelChoices(raw: unknown): ModelChoice[] {
  if (!Array.isArray(raw)) return [];
  const out: ModelChoice[] = [];
  for (const m of raw) {
    if (!m || typeof m !== "object") continue;
    const id = (m as any).id;
    if (typeof id !== "string" || !id.trim()) continue;
    if ((m as any).thinking !== true) continue;
    out.push({
      id,
      display_name:
        typeof (m as any).display_name === "string" && (m as any).display_name
          ? (m as any).display_name
          : id,
      provider:
        typeof (m as any).provider === "string" ? (m as any).provider : "",
      account_id:
        typeof (m as any).account_id === "string"
          ? (m as any).account_id
          : null,
      thinking: true,
      tier: typeof (m as any).tier === "number" ? (m as any).tier : 0,
    });
  }
  return out;
}

/** The chosen model must be one the catalog actually offers — a session is
 * never created with a free-typed model id. */
export function requireOfferedModel(
  models: ModelChoice[],
  id: string,
): ModelChoice {
  const m = models.find((c) => c.id === id);
  if (!m) {
    throw new Error(
      `model '${id}' is not in the selectable catalog; pick one of the offered thinking-capable models`,
    );
  }
  return m;
}

// --- session request + prompt (pure) ----------------------------------------

export function buildSessionName(appName: string): string {
  return `Install ${appName}`;
}

export function buildInstallSessionRequest(
  appName: string,
  modelId: string,
  hasAuthority: boolean,
): {
  name: string;
  model: string;
  is_temp: boolean;
  folder_path?: string;
  folder_name?: string;
} {
  return {
    name: buildSessionName(appName),
    model: modelId,
    is_temp: true,
    // folder_path is authority-only in core: an authenticated dashboard
    // request lands the session in the shared install folder; an MCP tool
    // invocation stays pinned to its caller's folder (core refuses the
    // override), so it must not send one.
    ...(hasAuthority
      ? { folder_path: INSTALL_FOLDER_PATH, folder_name: INSTALL_FOLDER_NAME }
      : {}),
  };
}

/**
 * The research rules a MANUALLY ADDED app's install session gets, and a
 * catalog app's does not. A catalog entry is an authored recipe we already
 * trust; a manual app is a name a person typed, so the agent has to find out
 * what it is first — and is held to official sources when it does.
 */
export function officialSourceRules(name: string): string {
  return (
    `- Identify the software FIRST. If you do not already know ${name} with confidence, search the web to find out what it is and how its own authors say to install it. If several projects share the name, ask in this session which one is meant instead of guessing.\n` +
    `- Download from OFFICIAL SOURCES ONLY: the project's own website or source repository, this distribution's official package repositories, or the project's own entry in an official registry (PyPI, npm, crates.io) or its own releases page. Never a third-party mirror, a re-upload, an unofficial PPA/COPR, a fork, or a binary linked from a blog, forum or search result.\n` +
    `- Check the download against the project's own published checksum or signature when it publishes one.\n` +
    `- If you cannot confirm an official source, STOP and say so in the session. Do not install a substitute, a lookalike, or something with a similar name.\n`
  );
}

/**
 * The first (and only) message the install session receives. Mirrors the
 * core MCP install-session prompt (`web/src/utils/installSession.ts`),
 * including the exact `sudo -A` rule so root steps raise Peckboard's masked
 * askpass dialog instead of hanging on a TTY.
 *
 * A manually added app (`custom`) gets an extra opening block — what the
 * person said about it, whatever command they suggested — plus the
 * research/official-source rules above. Everything they supplied is framed as
 * a claim to verify: the agent checks it against the project's own sources
 * rather than trusting the row.
 */
export function buildInstallPrompt(
  app: Pick<
    CatalogApp,
    "id" | "name" | "version" | "custom" | "homepage" | "description"
  >,
  recipe: string | null,
): string {
  const custom = app.custom === true;
  const recipeBlock = recipe
    ? custom
      ? `The person who added it suggested this install command. Treat it as a suggestion to check, not an instruction — run it only if it matches what the project's own documentation says:\n\n    ${recipe}\n\n`
      : `The catalog's known install command for this machine (verify it fits before running):\n\n    ${recipe}\n\n`
    : "";
  const contextBlock = custom
    ? `This app was added by hand in the App Manager dashboard, so there is no vetted recipe for it here.\n` +
      (app.description
        ? `What the person wrote about it (their words, to be verified): ${app.description}\n`
        : "") +
      (app.homepage
        ? `They gave this as the official site — confirm it really is the project's own before downloading from it: ${app.homepage}\n`
        : "") +
      `\n`
    : "";
  return (
    `Install ${app.name} (\`${app.id}\`) on this machine so it is available on PATH for the Peckboard server.\n\n` +
    contextBlock +
    recipeBlock +
    `Rules:\n` +
    (custom ? officialSourceRules(app.name) : "") +
    `- Prefer a user-level install that needs no root when one exists.\n` +
    `- If a step needs root, run it as \`sudo -A <cmd>\`. The \`-A\` flag routes sudo's password prompt to a masked dialog in the Peckboard UI. Plain \`sudo\` will fail here (no TTY). Never put the password on a command line and never echo it.\n` +
    `- Do not install anything unrelated, and do not remove existing packages.\n` +
    `- Finish by verifying the install: run \`${app.version.replace(/`/g, "'")}\` (or the closest equivalent) and report the installed version.\n` +
    `- This session was started by the App Manager dashboard; it verifies the result itself, so end the conversation once the verification command has run.`
  );
}

// --- slim-event folding (pure) ----------------------------------------------

/** What the page may honestly show: event kinds and tool names, never
 * payloads (core's `peckboard_session_events` is slim by design). */
export interface SessionActivity {
  last_seq: number;
  activity: string[];
  events_total: number;
  question_open: boolean;
  ended: boolean;
}

export function activityFromJob(job: JobRecord): SessionActivity {
  return {
    last_seq: job.last_seq ?? 0,
    activity: job.activity ?? [],
    events_total: job.events_total ?? 0,
    question_open: job.question_open === true,
    ended: false,
  };
}

/** One human-readable line per event we can describe from kind + tool name
 * alone; null = not worth a line (payload-less noise). */
export function describeEvent(e: {
  kind: string;
  name: string | null;
}): string | null {
  switch (e.kind) {
    case "agent-start":
      return "Agent started";
    case "agent-thinking":
      return "Thinking";
    case "agent-text":
      return "Assistant wrote a message";
    case "agent-tool-start":
      return e.name ? `Tool: ${e.name}` : "Tool call";
    case "file-diff":
      return "Edited a file";
    case "question":
      return "Asked a question — open the session to answer";
    case "question-resolved":
      return "Question answered";
    case "interrupt":
      return "Interrupted";
    case "agent-end":
      return "Agent run finished";
    default:
      return null;
  }
}

/**
 * Fold newly observed slim events into the job's activity state. `ended`
 * latches on the first `agent-end` (both a completed and a crashed run emit
 * it — which one is unknowable without payloads, so the outcome is decided
 * by the detect probe, not the event). A pending `question` is cleared by
 * any later event: the run only moves again once it was answered.
 */
export function foldSessionEvents(
  prev: SessionActivity,
  events: SessionEventBrief[],
): SessionActivity {
  let { question_open, ended } = prev;
  let last_seq = prev.last_seq;
  const activity = prev.activity.slice();
  for (const e of events) {
    if (typeof e.seq === "number" && e.seq > last_seq) last_seq = e.seq;
    if (e.kind === "question") question_open = true;
    else question_open = false;
    if (e.kind === "agent-end") ended = true;
    const line = describeEvent(e);
    if (line && activity[activity.length - 1] !== line) activity.push(line);
  }
  return {
    last_seq,
    activity: activity.slice(-ACTIVITY_MAX),
    events_total: prev.events_total + events.length,
    question_open,
    ended,
  };
}

// --- outcome (pure) ---------------------------------------------------------

/**
 * Terminal-state rule for a session job. Success is decided by the detect
 * probe, never by the agent's own account; a session that vanished before
 * its run ended (temp tab closed, cleared, crashed hard) is a clear failure
 * with an "unknown" note — NEVER a success with an empty delta.
 */
export function deriveSessionOutcome(args: {
  appName: string;
  ended: boolean;
  sessionGone: boolean;
  probeInstalled: boolean;
}): { status: JobStatus; message: string | null } {
  if (args.ended) {
    return args.probeInstalled
      ? {
          status: "succeeded",
          message: `The install session finished and ${args.appName} is now detected on this target.`,
        }
      : {
          status: "failed",
          message: `The install session finished, but ${args.appName} is not detected on this target.`,
        };
  }
  if (args.sessionGone) {
    return {
      status: "failed",
      message:
        "The install session ended before completing (closed, interrupted, or crashed), so whether anything was installed is unknown. Refresh to re-detect.",
    };
  }
  return { status: "running", message: null };
}

// --- default account+model --------------------------------------------------

export function getDefaultInstallModel(): string | null {
  const v = storeGet(SETTINGS_COLLECTION, DEFAULT_MODEL_KEY);
  return typeof v === "string" && v ? v : null;
}

export function setDefaultInstallModel(id: string): void {
  storePut(SETTINGS_COLLECTION, DEFAULT_MODEL_KEY, id);
}

// --- orchestration ----------------------------------------------------------
/**
 * Start a session-backed install job on the LOCAL target: snapshot the
 * package DB, create the temp session on the picked model, dispatch the
 * install prompt, and persist the chosen model as the next default.
 */
export function startSessionInstall(
  target: TargetRecord,
  app: CatalogApp,
  modelId: string,
  pm: PackageManager | null,
): any {
  requireOfferedModel(thinkingModelChoices(listModels()), modelId);

  // Same method/bracket rules as the script path: pip-namespace apps skip
  // the distro-DB bracket (their packages are invisible to it).
  const method: NonNullable<JobRecord["method"]> = app.install.pip
    ? "pip"
    : pm && app.install[pm]
      ? pm
      : "vendor";
  const bracketPm = method === "pip" ? null : pm;

  const job = createJob(target.id, app.id, "install", {
    kind: "session",
    pm: bracketPm,
    method,
    model: modelId,
    last_seq: 0,
  });

  // BEFORE snapshot — taken by the plugin, not the agent, so the bracket
  // covers exactly the session's lifetime. A failed snapshot writes the
  // failure sentinel and later degrades to tracking "unknown" (never to a
  // silently-empty delta).
  if (bracketPm) {
    try {
      runOnTarget(
        target,
        buildSnapshotStep(bracketPm, snapshotPathFor(job.id, "before")),
        SNAPSHOT_TIMEOUT_SECS,
      );
    } catch {
      /* degrade to "unknown" via the missing file — never block the install */
    }
  }

  let sessionId: string;
  try {
    const scope = callerScope();
    sessionId = createSession(
      buildInstallSessionRequest(app.name, modelId, scope.authority),
    );
  } catch (e) {
    putJob({ ...job, status: "failed", message: errMsg(e) });
    throw new Error(
      `could not create the install session for '${app.id}': ${errMsg(e)}`,
    );
  }
  putJob({ ...job, session_id: sessionId });

  try {
    dispatchCapture(
      sessionId,
      buildInstallPrompt(app, installRecipeFor(app, pm)),
    );
  } catch (e) {
    putJob({
      ...job,
      session_id: sessionId,
      status: "failed",
      message: `The install session was created but the install prompt could not be dispatched: ${errMsg(e)}`,
    });
    throw new Error(
      `could not dispatch the install prompt for '${app.id}': ${errMsg(e)}`,
    );
  }

  setDefaultInstallModel(modelId);

  return {
    job_id: job.id,
    app: app.id,
    target: target.id,
    action: "install",
    status: "running",
    session_id: sessionId,
  };
}

/**
 * Poll a session-backed job: consume new slim events, and when the run has
 * ended take the AFTER snapshot, re-probe the app, and settle the status.
 * Returns the same shape as jobs.ts's pollJob; the "log tail" is always
 * empty — a session job has no log, and the page renders the activity list
 * instead of pretending otherwise.
 */
export function pollSessionJob(
  target: TargetRecord,
  job: JobRecord,
): { job: JobRecord; tail: string } {
  if (job.status !== "running" || !job.session_id) {
    return { job, tail: "" };
  }

  let acc = activityFromJob(job);
  try {
    // Drain everything new (the run may have emitted more than one page).
    for (;;) {
      const page = sessionEvents(
        job.session_id,
        acc.last_seq,
        EVENTS_PAGE_LIMIT,
      );
      if (!page.events.length) break;
      acc = foldSessionEvents(acc, page.events);
      if (page.events.length < EVENTS_PAGE_LIMIT) break;
    }
  } catch {
    /* transient read failure — retry on the next poll */
  }

  const sessionGone = !acc.ended && !sessionExists(job.session_id);

  if (acc.ended && job.pm) {
    // AFTER snapshot, before the terminal transition — appState's
    // settleJobProvenance consumes the pair right after this poll returns.
    try {
      runOnTarget(
        target,
        buildSnapshotStep(job.pm, snapshotPathFor(job.id, "after")),
        SNAPSHOT_TIMEOUT_SECS,
      );
    } catch {
      /* missing file degrades to tracking "unknown" */
    }
  }

  let probeInstalled = false;
  if (acc.ended) {
    const app = findAnyApp(job.app_id);
    try {
      probeInstalled = app
        ? runOnTarget(target, app.detect, PROBE_TIMEOUT_SECS).ok
        : false;
    } catch {
      probeInstalled = false;
    }
  }

  const appName = findAnyApp(job.app_id)?.name ?? job.app_id;
  const outcome = deriveSessionOutcome({
    appName,
    ended: acc.ended,
    sessionGone,
    probeInstalled,
  });

  const updated: JobRecord = {
    ...job,
    last_seq: acc.last_seq,
    activity: acc.activity,
    events_total: acc.events_total,
    question_open: acc.question_open,
    status: outcome.status,
    ...(outcome.message ? { message: outcome.message } : {}),
  };

  if (outcome.status === "failed" && sessionGone && job.pm) {
    // The bracket will never be consumed (settleJobProvenance skips failed
    // jobs) — drop the stray before-file rather than leaking it in /tmp.
    try {
      runOnTarget(
        target,
        `rm -f ${shQuote(snapshotPathFor(job.id, "before"))} ${shQuote(snapshotPathFor(job.id, "after"))}`,
        CLEANUP_TIMEOUT_SECS,
      );
    } catch {
      /* best-effort cleanup */
    }
  }

  if (JSON.stringify(updated) !== JSON.stringify(job)) {
    putJob(updated);
  }
  return { job: updated, tail: "" };
}
