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

  const binary = String(input?.binary ?? existing?.binary ?? id).trim() || id;
  if (!BINARY_RE.test(binary)) {
    throw new Error(
      `'${binary}' is not a valid command name (letters, digits and . _ + - only)`,
    );
  }

  const notes = String(input?.notes ?? existing?.notes ?? "").trim();
  if (notes.length > MAX_DESCRIPTION) {
    throw new Error(`notes are too long (max ${MAX_DESCRIPTION} characters)`);
  }

  const homepageRaw = String(
    input?.homepage ?? existing?.homepage ?? "",
  ).trim();
  if (homepageRaw && !/^https:\/\/[^\s"'<>]+$/.test(homepageRaw)) {
    throw new Error(
      "the official website must be an https:// URL (leave it blank if you don't know it)",
    );
  }

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
    ...(homepageRaw ? { homepage: homepageRaw } : {}),
    ...(installCommand ? { install_command: installCommand } : {}),
    ...(removeCommand ? { remove_command: removeCommand } : {}),
    created_at: existing?.created_at ?? new Date().toISOString(),
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
 * are the user's own commands, present only when they typed them (a `vendor`
 * recipe, since a manual app has no per-package-manager form).
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
