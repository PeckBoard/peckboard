// Filling in a manually added app's blanks with an AI session.
//
// A manual app starts as a name someone typed: no notes, no official site, no
// install or remove command, and a detect binary guessed from the id. This
// module runs a TEMPORARY session that works out what the software actually
// is and reports back.
//
// The one structural constraint that shapes everything here: a plugin can
// create and dispatch a session, but `peckboard_session_events` is SLIM
// ({seq, kind, name}, never payloads). This plugin therefore cannot read what
// the agent wrote — so findings do not come back through the transcript, they
// come back through the `app_record_details` MCP tool this plugin provides
// (tools.ts). The session's event tail is used for one thing only: knowing
// whether the run is still going.
//
// What comes back is filtered by customApps.ts's `applyResearchDetails`:
// blanks only, and an install/remove command lands as a SUGGESTION a person
// accepts in the dashboard rather than as live shell. The research session
// installs nothing — that is a separate, explicit action on the row.

import {
  CustomAppRecord,
  ResearchState,
  getCustomApp,
  missingDetails,
  pendingSuggestions,
  putCustomApp,
} from "./customApps";
import {
  SessionEventBrief,
  callerScope,
  createSession,
  dispatchCapture,
  listModels,
  sessionEvents,
  sessionExists,
} from "./host";
import {
  DETAILS_TOOL,
  INSTALL_FOLDER_NAME,
  INSTALL_FOLDER_PATH,
  foldSessionEvents,
  getDefaultInstallModel,
  officialSourceRules,
  requireOfferedModel,
  thinkingModelChoices,
} from "./installSession";
import { errMsg } from "./verdict";

/** The MCP tool the session reports its findings through (defined next to the
 * install prompt that also uses it, so the two can never drift apart). */
export { DETAILS_TOOL };
const EVENTS_PAGE_LIMIT = 200;

// --- session request + prompt (pure) ----------------------------------------

export function buildResearchSessionName(appName: string): string {
  return `Research ${appName}`;
}

export function buildResearchSessionRequest(
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
    name: buildResearchSessionName(appName),
    model: modelId,
    is_temp: true,
    // Same authority rule as the install session: only an authenticated
    // dashboard request may place the session in the shared folder.
    ...(hasAuthority
      ? { folder_path: INSTALL_FOLDER_PATH, folder_name: INSTALL_FOLDER_NAME }
      : {}),
  };
}

/**
 * The research prompt. It is explicitly a read-only errand — the session must
 * not install anything — and it ends by calling `app_record_details`, which
 * is the only way its findings reach this plugin.
 */
export function buildResearchPrompt(rec: CustomAppRecord): string {
  const blanks = missingDetails(rec);
  const claims =
    (rec.notes
      ? `What the person wrote about it (their words, to be verified): ${rec.notes}\n`
      : "") +
    (rec.homepage
      ? `They gave this as the official site — confirm it really is the project's own: ${rec.homepage}\n`
      : "") +
    (rec.install_command
      ? `They already typed this install command, so do not propose one: ${rec.install_command}\n`
      : "") +
    (rec.remove_command
      ? `They already typed this remove command, so do not propose one: ${rec.remove_command}\n`
      : "");
  return (
    `Find out what "${rec.name}" (\`${rec.id}\`) is, and record what you find in Peckboard's App Manager. ` +
    `It was added by hand there, so the entry is missing: ${blanks.join(", ")}.\n\n` +
    claims +
    `\nRules:\n` +
    `- DO NOT INSTALL ANYTHING and do not change this machine. This session only researches; installing is a separate action a person takes on the row.\n` +
    officialSourceRules(rec.name) +
    `- Work out the install and removal commands this Linux host's own documentation would use, from the project's or the distribution's own instructions. Prefer a user-level install that needs no root; if a step needs root, write it as \`sudo -A <cmd>\` (that is how Peckboard routes the password prompt).\n` +
    `- Each command must be a SINGLE LINE and no more than 500 characters.\n` +
    `- Finish by calling the \`${DETAILS_TOOL}\` tool with \`app\` set to \`${rec.id}\` and only the fields you actually established: \`binary\` (the executable that proves it is installed), \`homepage\`, \`notes\` (one short line on what the software is), \`install_command\`, \`remove_command\`.\n` +
    `- Anything the person already filled in is kept, not replaced, and the tool tells you which of your values it used. The install and remove commands you give are stored as SUGGESTIONS for a person to review and accept — nothing runs them because you proposed them.\n` +
    `- If you cannot identify the software with confidence, call the tool with only what you are sure of (or nothing at all) and say so in the session. A blank entry is better than a wrong one.\n` +
    `- This session was started by the App Manager dashboard; end the conversation once you have called the tool.`
  );
}

// --- outcome (pure) ---------------------------------------------------------

/**
 * Terminal-state rule for a research session. What it achieved is read from
 * the RECORD (what the tool actually wrote), never from the agent's account
 * of itself — a run that ended without calling the tool is reported as having
 * recorded nothing, not as a success.
 */
export function deriveResearchOutcome(args: {
  appName: string;
  ended: boolean;
  sessionGone: boolean;
  applied: string[];
  suggestedCount: number;
}): { status: ResearchState["status"]; message: string | null } {
  if (args.ended) {
    const parts: string[] = [];
    if (args.applied.length) {
      parts.push(`Filled in: ${args.applied.join(", ")}.`);
    }
    if (args.suggestedCount) {
      parts.push(
        args.suggestedCount === 1
          ? "1 command is waiting for you to review it."
          : `${args.suggestedCount} commands are waiting for you to review them.`,
      );
    }
    if (!parts.length) {
      return {
        status: "done",
        message: `The research session finished without recording any details for ${args.appName}.`,
      };
    }
    return { status: "done", message: parts.join(" ") };
  }
  if (args.sessionGone) {
    return {
      status: "failed",
      message:
        "The research session ended before recording anything (closed, interrupted, or crashed). Nothing was changed.",
    };
  }
  return { status: "running", message: null };
}

// --- orchestration ----------------------------------------------------------

/**
 * Start a research session for a manual app and persist the running state.
 * Throws a user-facing sentence if the model isn't selectable or the session
 * can't be created — the dashboard shows it next to the button that asked.
 */
export function startResearch(
  rec: CustomAppRecord,
  modelId: string,
): CustomAppRecord {
  requireOfferedModel(thinkingModelChoices(listModels()), modelId);

  let sessionId: string;
  try {
    const scope = callerScope();
    sessionId = createSession(
      buildResearchSessionRequest(rec.name, modelId, scope.authority),
    );
  } catch (e) {
    throw new Error(
      `could not create the research session for '${rec.id}': ${errMsg(e)}`,
    );
  }

  const started: CustomAppRecord = {
    ...rec,
    research: {
      status: "running",
      session_id: sessionId,
      model: modelId,
      started_at: new Date().toISOString(),
      last_seq: 0,
    },
  };
  putCustomApp(started);

  try {
    dispatchCapture(sessionId, buildResearchPrompt(rec));
  } catch (e) {
    const failed: CustomAppRecord = {
      ...started,
      research: {
        ...started.research!,
        status: "failed",
        finished_at: new Date().toISOString(),
        message: `The research session was created but the prompt could not be dispatched: ${errMsg(e)}`,
      },
    };
    putCustomApp(failed);
    throw new Error(
      `could not dispatch the research prompt for '${rec.id}': ${errMsg(e)}`,
    );
  }

  return started;
}

/**
 * Best-effort research when an app is saved with blanks: uses the model the
 * dashboard last installed with. Never blocks or fails the save — no model
 * chosen, or a session that won't start, just leaves the record as typed and
 * returns the reason, and the row still offers "Fill in details".
 */
export function maybeStartResearch(rec: CustomAppRecord): {
  rec: CustomAppRecord;
  note: string | null;
} {
  if (!missingDetails(rec).length) return { rec, note: null };
  const model = getDefaultInstallModel();
  if (!model) {
    return {
      rec,
      note: "No model has been chosen yet, so nothing was looked up — press “Fill in details” on the row to pick one.",
    };
  }
  try {
    return {
      rec: startResearch(rec, model),
      note: "A temporary AI session is looking the rest up now.",
    };
  } catch (e) {
    return { rec, note: `The details could not be looked up: ${errMsg(e)}` };
  }
}

/**
 * Poll a running research session and settle it when the run ends. Always
 * re-reads the stored record first: the findings arrive by MCP tool call
 * (tools.ts writes them), so the caller's copy may already be stale.
 */
export function pollResearch(rec: CustomAppRecord): CustomAppRecord {
  const current = getCustomApp(rec.id) ?? rec;
  const research = current.research;
  if (!research || research.status !== "running" || !research.session_id) {
    return current;
  }

  let acc = {
    last_seq: research.last_seq ?? 0,
    activity: [] as string[],
    events_total: 0,
    question_open: false,
    ended: false,
  };
  try {
    for (;;) {
      const page = sessionEvents(
        research.session_id,
        acc.last_seq,
        EVENTS_PAGE_LIMIT,
      );
      if (!page.events.length) break;
      acc = foldSessionEvents(acc, page.events as SessionEventBrief[]);
      if (page.events.length < EVENTS_PAGE_LIMIT) break;
    }
  } catch {
    /* transient read failure — retry on the next poll */
  }

  const sessionGone = !acc.ended && !sessionExists(research.session_id);
  const outcome = deriveResearchOutcome({
    appName: current.name,
    ended: acc.ended,
    sessionGone,
    applied: current.filled_fields ?? [],
    suggestedCount: pendingSuggestions(current).length,
  });

  const updated: CustomAppRecord = {
    ...current,
    research: {
      ...research,
      last_seq: acc.last_seq,
      status: outcome.status,
      ...(outcome.status === "running"
        ? {}
        : { finished_at: new Date().toISOString() }),
      ...(outcome.message ? { message: outcome.message } : {}),
    },
  };

  if (JSON.stringify(updated) !== JSON.stringify(current)) {
    putCustomApp(updated);
  }
  return updated;
}
