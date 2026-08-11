// Manually added apps: the ones the catalog doesn't have. A person names the
// software in the dashboard, and it becomes a row alongside the catalog apps.
//
// How this differs from catalog.ts, deliberately:
//
//   - There is no authored recipe. On the LOCAL host the install runs through
//     the same temporary AI session catalog apps use, and the agent works out
//     the method — searching the web when it doesn't know the software, and
//     downloading only from official sources (see installSession.ts's prompt).
//   - Remote targets have no AI session available (it runs on the Peckboard
//     host and has no path to a target's SSH credentials), so a remote install
//     needs an `install_command` the PERSON typed. That command is
//     user-authored shell run verbatim on the target — the one place this
//     plugin runs something that isn't a static catalog recipe. It is only
//     creatable from the authenticated dashboard, and the page shows it back
//     verbatim before the first run.
//   - Everything else the plugin derives (the detect and version probes) is
//     built from a validated `binary` token and shell-quoted anyway, so a
//     stored record can never smuggle a command into a probe.
//
// A record that arrives with blanks gets them filled in by an AI session (a
// research session on save, and the install session on its way out — see
// researchSession.ts). Two rules keep that honest, both enforced in
// `applyResearchDetails` below:
//
//   - It only ever fills a BLANK. Anything the person typed is left alone;
//     what a session filled in is listed in `filled_fields`.
//   - An install/remove command from a session is NOT a command. It lands in
//     `suggested_*`, which nothing runs, and becomes real only when someone
//     accepts it in the dashboard — the same verbatim preview a typed command
//     already gets. An agent must not be able to arm shell that later runs on
//     a remote target.
//
// The record is projected into the CatalogApp shape (`toCatalogApp`) so rows,
// jobs, probes and provenance need no special casing downstream.

import { APPS, CatalogApp, findApp } from "./catalog";
import { storeDelete, storeGet, storeList, storePut } from "./host";
import { shQuote } from "./jobs";

const COLLECTION = "custom_apps";

const MAX_NAME = 64;
const MAX_DESCRIPTION = 400;
const MAX_COMMAND = 500;
/** Command names are compared and probed as a bare token, never quoted into
 * an expression — keep them to what a real executable name can be. */
const BINARY_RE = /^[A-Za-z0-9._+-]+$/;
const ID_RE = /^[a-z0-9][a-z0-9-]*$/;

/** The state of the AI session filling this app's blanks in. Driven by
 * researchSession.ts; stored on the record so the row can say what's
 * happening without a second collection to keep in sync. */
export interface ResearchState {
  status: "running" | "done" | "failed";
  session_id?: string;
  model?: string;
  started_at: string;
  finished_at?: string;
  /** One sentence for the page: how it ended, or what it recorded. */
  message?: string;
  /** Slim-event cursor — see installSession.ts's folding. */
  last_seq?: number;
}

export interface CustomAppRecord {
  id: string;
  name: string;
  /** The executable the probe looks for on PATH. */
  binary: string;
  /** The user's own note about what this is — shown on the row and handed to
   * the install session as context to verify, never as truth. */
  notes: string;
  /** Official project URL, https only. A claim for the agent to check. */
  homepage?: string;
  /** User-authored shell, run verbatim on remote targets. Absent = the app
   * can only be installed on the local host, via the AI session. */
  install_command?: string;
  /** User-authored shell, run verbatim to uninstall. Absent = the row offers
   * Forget only; this plugin never guesses a removal command. */
  remove_command?: string;
  /** True when `binary` was derived from the id instead of typed. The one
   * field a session may correct — a record saved before this flag existed
   * counts as typed, so an upgrade never overwrites someone's own value. */
  binary_derived?: boolean;
  /** Shell an AI session proposed. Deliberately NOT `install_command`:
   * nothing runs this until a person accepts it in the dashboard. */
  suggested_install_command?: string;
  suggested_remove_command?: string;
  /** Field labels an AI session filled in, for the row's note. */
  filled_fields?: string[];
  research?: ResearchState;
  created_at?: string;
}

/** Slugify a display name into a stable id. Returns "" when nothing usable
 * survives (e.g. a name written entirely in a non-latin script) — the caller
 * turns that into a "give the app an id" error rather than minting one. */
export function slugify(name: string): string {
  return name
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 40)
    .replace(/-+$/g, "");
}

function optionalCommand(v: unknown, field: string): string | undefined {
  const s = String(v ?? "").trim();
  if (!s) return undefined;
  if (s.length > MAX_COMMAND) {
    throw new Error(`${field} is too long (max ${MAX_COMMAND} characters)`);
  }
  if (/[\r\n]/.test(s)) {
    throw new Error(`${field} must be a single line`);
  }
  return s;
}

function checkBinary(binary: string): string {
  if (!BINARY_RE.test(binary)) {
    throw new Error(
      `'${binary}' is not a valid command name (letters, digits and . _ + - only)`,
    );
  }
  return binary;
}

function checkNotes(notes: string): string {
  if (notes.length > MAX_DESCRIPTION) {
    throw new Error(`notes are too long (max ${MAX_DESCRIPTION} characters)`);
  }
  return notes;
}

function checkHomepage(homepage: string): string {
  if (homepage && !/^https:\/\/[^\s"'<>]+$/.test(homepage)) {
    throw new Error(
      "the official website must be an https:// URL (leave it blank if you don't know it)",
    );
  }
  return homepage;
}

/**
 * Validate a submitted manual app into a stored record. Throws a plain Error
 * with a user-facing sentence on any invalid input; `existing` set means an
 * edit, so unspecified fields keep their stored value and the id is fixed.
 */
export function buildCustomApp(
  input: any,
  existing: CustomAppRecord | null = null,
  taken: (id: string) => boolean = (id) => !!findApp(id) || !!getCustomApp(id),
): CustomAppRecord {
  const name = String(input?.name ?? existing?.name ?? "").trim();
  if (!name) throw new Error("name is required");
  if (name.length > MAX_NAME) {
    throw new Error(`name is too long (max ${MAX_NAME} characters)`);
  }

  const id = existing?.id ?? slugify(String(input?.id ?? name));
  if (!id || !ID_RE.test(id)) {
    throw new Error(
      `'${name}' does not give a usable id (letters, digits and dashes); type one in the id field`,
    );
  }
  if (!existing && taken(id)) {
    throw new Error(
      `an app with the id '${id}' already exists — pick a different name or id`,
    );
  }

  // A typed binary is theirs; a blank one is derived from the id and stays
  // fair game for a research session to correct.
  const typedBinary = String(input?.binary ?? "").trim();
  let binary: string;
  let binaryDerived: boolean;
  if (typedBinary) {
    binary = typedBinary;
    binaryDerived = false;
  } else if (input?.binary === undefined && existing?.binary) {
    binary = existing.binary;
    binaryDerived = existing.binary_derived === true;
  } else {
    binary = id;
    binaryDerived = true;
  }
  checkBinary(binary);

  const notes = checkNotes(
    String(input?.notes ?? existing?.notes ?? "").trim(),
  );
  const homepageRaw = checkHomepage(
    String(input?.homepage ?? existing?.homepage ?? "").trim(),
  );

  const installCommand =
    input?.install_command === undefined
      ? existing?.install_command
      : optionalCommand(input.install_command, "the install command");
  const removeCommand =
    input?.remove_command === undefined
      ? existing?.remove_command
      : optionalCommand(input.remove_command, "the remove command");

  return {
    id,
    name,
    binary,
    notes,
    ...(binaryDerived ? { binary_derived: true } : {}),
    ...(homepageRaw ? { homepage: homepageRaw } : {}),
    ...(installCommand ? { install_command: installCommand } : {}),
    ...(removeCommand ? { remove_command: removeCommand } : {}),
    // A suggestion the person hasn't answered yet survives an edit, unless
    // this edit typed the very command it was suggesting.
    ...(existing?.suggested_install_command && !installCommand
      ? { suggested_install_command: existing.suggested_install_command }
      : {}),
    ...(existing?.suggested_remove_command && !removeCommand
      ? { suggested_remove_command: existing.suggested_remove_command }
      : {}),
    ...(existing?.filled_fields?.length
      ? { filled_fields: existing.filled_fields }
      : {}),
    ...(existing?.research ? { research: existing.research } : {}),
    created_at: existing?.created_at ?? new Date().toISOString(),
  };
}

// --- filling the blanks in --------------------------------------------------

/** What a research or install session reports back through the
 * `app_record_details` MCP tool. Every field optional: an agent records only
 * what it actually established. */
export interface DetailFindings {
  binary?: unknown;
  homepage?: unknown;
  notes?: unknown;
  install_command?: unknown;
  remove_command?: unknown;
}

export interface DetailOutcome {
  rec: CustomAppRecord;
  /** Blank fields now filled in. */
  applied: string[];
  /** Commands parked for a person to accept — nothing runs them yet. */
  suggested: string[];
  /** Fields left exactly as they were, and why-by-label. */
  skipped: string[];
}

/** The human labels used in `filled_fields`, the tool's reply and the row. */
const LABEL = {
  binary: "detect command",
  homepage: "official website",
  notes: "notes",
  install_command: "install command",
  remove_command: "remove command",
};

/**
 * Merge an AI session's findings into a stored record.
 *
 * Two invariants, and they are the whole point of this function:
 *
 *   1. It only fills BLANKS. A field the person typed is never overwritten —
 *      it comes back in `skipped` instead, so the agent is told plainly that
 *      its value was not used.
 *   2. An install/remove command NEVER lands live. It goes to `suggested_*`,
 *      which no code path runs, pending someone accepting it in the dashboard
 *      (`acceptSuggestion`). This plugin runs user-authored shell verbatim on
 *      remote targets; an agent must not be able to author that on its own.
 *
 * Invalid input throws the same user-facing sentences `buildCustomApp` uses —
 * the agent sees the message as its tool result and can correct itself.
 */
export function applyResearchDetails(
  rec: CustomAppRecord,
  findings: DetailFindings,
): DetailOutcome {
  const out: CustomAppRecord = { ...rec };
  const applied: string[] = [];
  const suggested: string[] = [];
  const skipped: string[] = [];

  const notes = String(findings.notes ?? "").trim();
  if (notes) {
    if (out.notes) skipped.push(LABEL.notes);
    else {
      out.notes = checkNotes(notes);
      applied.push(LABEL.notes);
    }
  }

  const homepage = String(findings.homepage ?? "").trim();
  if (homepage) {
    checkHomepage(homepage);
    if (out.homepage) skipped.push(LABEL.homepage);
    else {
      out.homepage = homepage;
      applied.push(LABEL.homepage);
    }
  }

  const binary = String(findings.binary ?? "").trim();
  if (binary) {
    checkBinary(binary);
    if (out.binary_derived !== true) skipped.push(LABEL.binary);
    else if (binary === out.binary) {
      // Already what the probe uses — nothing to report as a change, but it
      // is now a checked value rather than a guess off the id.
      delete out.binary_derived;
    } else {
      out.binary = binary;
      delete out.binary_derived;
      applied.push(LABEL.binary);
    }
  }

  const install = optionalCommand(
    findings.install_command,
    "the install command",
  );
  if (install) {
    if (out.install_command) skipped.push(LABEL.install_command);
    else {
      out.suggested_install_command = install;
      suggested.push(LABEL.install_command);
    }
  }

  const remove = optionalCommand(findings.remove_command, "the remove command");
  if (remove) {
    if (out.remove_command) skipped.push(LABEL.remove_command);
    else {
      out.suggested_remove_command = remove;
      suggested.push(LABEL.remove_command);
    }
  }

  if (applied.length) {
    const merged = (out.filled_fields ?? []).slice();
    applied.forEach((f) => {
      if (merged.indexOf(f) < 0) merged.push(f);
    });
    out.filled_fields = merged;
  }

  return { rec: out, applied, suggested, skipped };
}

export type SuggestionField = "install" | "remove";

function suggestionKey(
  field: SuggestionField,
): "suggested_install_command" | "suggested_remove_command" {
  return field === "install"
    ? "suggested_install_command"
    : "suggested_remove_command";
}

/** The suggested commands still awaiting an answer. */
export function pendingSuggestions(
  rec: CustomAppRecord,
): Array<{ field: SuggestionField; label: string; command: string }> {
  const out: Array<{
    field: SuggestionField;
    label: string;
    command: string;
  }> = [];
  if (rec.suggested_install_command) {
    out.push({
      field: "install",
      label: LABEL.install_command,
      command: rec.suggested_install_command,
    });
  }
  if (rec.suggested_remove_command) {
    out.push({
      field: "remove",
      label: LABEL.remove_command,
      command: rec.suggested_remove_command,
    });
  }
  return out;
}

/** Accept a suggested command: THIS is the step that makes it runnable, and
 * it only happens from an authenticated dashboard request. */
export function acceptSuggestion(
  rec: CustomAppRecord,
  field: SuggestionField,
): CustomAppRecord {
  const key = suggestionKey(field);
  const command = rec[key];
  if (!command) {
    throw new Error(
      `there is no suggested ${field} command for '${rec.id}' to accept`,
    );
  }
  const out: CustomAppRecord = { ...rec };
  if (field === "install") out.install_command = command;
  else out.remove_command = command;
  delete out[key];
  const label =
    field === "install" ? LABEL.install_command : LABEL.remove_command;
  const merged = (out.filled_fields ?? []).slice();
  if (merged.indexOf(label) < 0) merged.push(label);
  out.filled_fields = merged;
  return out;
}

/** Discard a suggested command. The field stays blank — it was never live. */
export function discardSuggestion(
  rec: CustomAppRecord,
  field: SuggestionField,
): CustomAppRecord {
  const key = suggestionKey(field);
  if (!rec[key]) {
    throw new Error(
      `there is no suggested ${field} command for '${rec.id}' to discard`,
    );
  }
  const out: CustomAppRecord = { ...rec };
  delete out[key];
  return out;
}

/** Which entries are still blank, by label — what a research session is for.
 * The detect command counts only while it's a guess off the id. */
export function missingDetails(rec: CustomAppRecord): string[] {
  const missing: string[] = [];
  if (!rec.notes) missing.push(LABEL.notes);
  if (!rec.homepage) missing.push(LABEL.homepage);
  if (rec.binary_derived === true) missing.push(LABEL.binary);
  if (!rec.install_command && !rec.suggested_install_command) {
    missing.push(LABEL.install_command);
  }
  if (!rec.remove_command && !rec.suggested_remove_command) {
    missing.push(LABEL.remove_command);
  }
  return missing;
}

/** The record as the dashboard sees it: the stored fields, plus what is still
 * blank and which commands are waiting on an answer. Computed here so the
 * page never re-derives either rule and drifts from it. */
export function customAppView(rec: CustomAppRecord): any {
  return {
    ...rec,
    missing: missingDetails(rec),
    suggestions: pendingSuggestions(rec),
  };
}

/** The row description: the person's own note, or an honest default that says
 * what will happen rather than pretending to describe the software. */
export function describeCustomApp(rec: CustomAppRecord): string {
  const base =
    rec.notes ||
    "Manually added — not in the catalog. Installing on this host starts an AI session " +
      "that identifies the software (searching the web if needed) and installs it from an " +
      "official source only.";
  return rec.homepage ? `${base} Official site: ${rec.homepage}` : base;
}

/**
 * Project a stored record into the CatalogApp shape. The probes are built from
 * the validated `binary` token and shell-quoted; the install/remove recipes
 * are the user's own commands, present only when they typed them or accepted
 * a suggestion (a `vendor` recipe, since a manual app has no
 * per-package-manager form). A pending `suggested_*` is deliberately NOT
 * projected: an unanswered suggestion must never become a runnable recipe.
 */
export function toCatalogApp(rec: CustomAppRecord): CatalogApp {
  const q = shQuote(rec.binary);
  return {
    id: rec.id,
    name: rec.name,
    description: describeCustomApp(rec),
    custom: true,
    binary: rec.binary,
    homepage: rec.homepage,
    detect: `command -v ${q}`,
    version: `${q} --version 2>&1 || ${q} version 2>&1`,
    install: rec.install_command ? { vendor: rec.install_command } : {},
    remove: rec.remove_command ? { vendor: rec.remove_command } : {},
  };
}

// --- store ------------------------------------------------------------------

export function listCustomApps(): CustomAppRecord[] {
  return storeList(COLLECTION)
    .map((i) => i.value as CustomAppRecord)
    .filter((r) => r && typeof r.id === "string" && typeof r.name === "string")
    .sort((a, b) => a.name.localeCompare(b.name));
}

export function getCustomApp(id: string): CustomAppRecord | null {
  return (storeGet(COLLECTION, id) as CustomAppRecord) ?? null;
}

export function putCustomApp(rec: CustomAppRecord): void {
  storePut(COLLECTION, rec.id, rec);
}

/** Forget a manual app: drops the entry from this plugin's list. It does NOT
 * uninstall anything — the caller says so in the confirmation. */
export function forgetCustomApp(id: string): boolean {
  if (!getCustomApp(id)) return false;
  storeDelete(COLLECTION, id);
  return true;
}

/** Every manual app, in CatalogApp form. */
export function customCatalogApps(): CatalogApp[] {
  return listCustomApps().map(toCatalogApp);
}

/** One manual app in CatalogApp form, or undefined. */
export function findCustomApp(id: string): CatalogApp | undefined {
  const rec = getCustomApp(id);
  return rec ? toCatalogApp(rec) : undefined;
}

// --- the whole app set ------------------------------------------------------

/** Catalog apps first, then the manually added ones — the set every row,
 * probe and job resolves against. A manual app whose id a LATER catalog
 * release has taken is dropped here rather than rendered as a second row with
 * the same id: `findAnyApp` resolves that id to the catalog entry, so a
 * duplicate row would act on something other than what it names. */
export function allApps(): CatalogApp[] {
  return [...APPS, ...customCatalogApps().filter((a) => !findApp(a.id))];
}

/** Resolve an app id against the catalog, then the manual apps. */
export function findAnyApp(id: string): CatalogApp | undefined {
  return findApp(id) ?? findCustomApp(id);
}
