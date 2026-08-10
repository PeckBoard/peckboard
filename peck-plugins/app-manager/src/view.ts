// Pure view-model shaping for the dashboard page. The page itself is served
// as one self-contained HTML string (see page.ts) and therefore cannot import
// anything, so every decision that needs testing — what the installed badge
// says, whether an action button is enabled, how a raw error is turned into a
// sentence — is made HERE, server-side, and shipped to the page as plain data.
//
// No host calls, no DOM: this module loads under vitest as-is.

import { CatalogApp, installRecipeFor, removeRecipeFor } from "./catalog";
import { PackageManager } from "./distro";
import { JobRecord } from "./jobs";
import { InstallRecord, PkgRef } from "./provenance";
import { TargetRecord } from "./targets";

// --- targets ----------------------------------------------------------------

export interface TargetView {
  id: string;
  kind: string;
  label: string;
  /** "this Peckboard host" / "user@host:port" — the sub-line in the picker. */
  detail: string;
  hostname?: string;
  port?: number;
  username?: string;
  key_id?: string;
  known_host?: string;
}

export function targetView(t: TargetRecord): TargetView {
  const detail =
    t.kind === "local"
      ? "this Peckboard host"
      : `${t.username ?? "?"}@${t.hostname ?? "?"}:${t.port ?? 22}`;
  return {
    id: t.id,
    kind: t.kind,
    label: t.label,
    detail,
    hostname: t.hostname,
    port: t.port,
    username: t.username,
    key_id: t.key_id,
    known_host: t.known_host,
  };
}

// --- distro -----------------------------------------------------------------

const PM_LABEL: Record<PackageManager, string> = {
  apt: "apt (Debian/Ubuntu)",
  dnf: "dnf (Fedora/RHEL)",
  pacman: "pacman (Arch)",
  zypper: "zypper (SUSE)",
};

export interface DistroView {
  /** Can the app grid be shown at all? False = not a usable Linux target. */
  supported: boolean;
  id: string;
  package_manager: PackageManager | null;
  /** One-line summary for the banner. */
  summary: string;
  /** Set when `supported` is false: the refusal, in prose. */
  refusal: string | null;
}

/**
 * Shape the distro banner. `probeError` set = the target didn't answer with a
 * readable /etc/os-release, so it isn't a Linux target we can drive at all.
 * A readable release with no known package manager is still usable — detection
 * works, only install/remove recipes are missing.
 */
export function distroView(
  probe: { id: string; idLike: string[]; pm: PackageManager | null } | null,
  probeError: string | null,
): DistroView {
  if (!probe) {
    return {
      supported: false,
      id: "",
      package_manager: null,
      summary: "Not a supported Linux target",
      refusal: friendlyError(
        probeError || "the target did not return a readable /etc/os-release",
      ),
    };
  }
  const name = probe.id || "unknown";
  if (!probe.pm) {
    const like = probe.idLike.length
      ? ` (like ${probe.idLike.join(", ")})`
      : "";
    return {
      supported: true,
      id: probe.id,
      package_manager: null,
      summary: `${name}${like} — no supported package manager`,
      refusal: null,
    };
  }
  return {
    supported: true,
    id: probe.id,
    package_manager: probe.pm,
    summary: `${name} — ${PM_LABEL[probe.pm]}`,
    refusal: null,
  };
}

// --- app rows ---------------------------------------------------------------

export interface AppRowView {
  id: string;
  name: string;
  description: string;
  /** "pip" = Python package in pip's namespace, not a system package —
   * rendered as a distinct badge so the two can't be confused. */
  namespace: "system" | "pip";
  installed: boolean;
  version: string | null;
  /** "Installed" / "Not installed" — the badge text. */
  state_label: string;
  /** The one action offered for this row. */
  action: "install" | "remove";
  action_label: string;
  /** False when no recipe exists for this target's package manager. */
  actionable: boolean;
  /** Why `actionable` is false, in prose. */
  blocked_reason: string | null;
  /** The package-DB version of the app's own package from the last
   * recorded install, when it maps to one — authoritative for the package,
   * noted alongside the binary's probed version. */
  package_version: string | null;
  /** Provenance caveat, in prose: a vendor-script install isn't tracked by
   * the package manager; a failed snapshot bracket is "unknown"; a pip app
   * is tracked in pip's namespace, never the distro package DB. Rendered
   * explicitly — never as an empty package list. */
  provenance_note: string | null;
  /** "Installed with <app>" — the label over `added_packages`. */
  added_label: string | null;
  /** Packages recorded as arriving during this app's install job, each
   * with its package-DB version. Rendered visually secondary to the app. */
  added_packages: PkgRef[];
  /** The in-flight (or last) job for this app on this target, if any. */
  job: JobView | null;
}

export function appRowView(
  app: CatalogApp,
  probe: { installed: boolean; version: string | null },
  pm: PackageManager | null,
  job: JobRecord | null,
  record: InstallRecord | null = null,
): AppRowView {
  const action = probe.installed ? "remove" : "install";
  // A record for an app that's no longer installed (removed outside this
  // plugin) would present stale provenance as current — suppress it.
  const rec = probe.installed ? record : null;
  let provenanceNote: string | null = null;
  if (rec) {
    if (rec.tracking === "pip" || rec.method === "pip") {
      provenanceNote =
        rec.note ??
        "Python package installed via pip — tracked in pip's namespace, not the distro package database.";
    } else if (rec.tracking === "unknown") {
      provenanceNote = rec.note ?? "What this install added is unknown.";
    } else if (rec.method === "vendor") {
      provenanceNote =
        "Installed by a vendor script — not tracked by the package manager.";
    }
  }
  const added = rec && rec.tracking === "tracked" ? rec.added : [];
  const recipe = probe.installed
    ? removeRecipeFor(app, pm)
    : installRecipeFor(app, pm);
  return {
    id: app.id,
    name: app.name,
    description: app.description,
    namespace: app.namespace ?? "system",
    installed: probe.installed,
    version: probe.version,
    state_label: probe.installed ? "Installed" : "Not installed",
    action,
    action_label: probe.installed ? "Remove" : "Install",
    actionable: recipe !== null,
    blocked_reason: recipe
      ? null
      : `No ${action} recipe for ${app.name} on this distribution.`,
    package_version: rec?.primary?.version ?? null,
    provenance_note: provenanceNote,
    added_label: added.length ? `Installed with ${app.name}` : null,
    added_packages: added,
    job: job ? jobView(job, "") : null,
  };
}

// --- jobs -------------------------------------------------------------------

export interface JobView {
  id: string;
  action: "install" | "remove";
  status: "running" | "succeeded" | "failed";
  /** Headline for the log panel, e.g. "Installing… ". */
  label: string;
  /** Styling hint the page maps to a colour: neutral | ok | bad. */
  tone: "busy" | "ok" | "bad";
  exit_code: number | null;
  log_tail: string;
  /** True for AI-session installs — the page renders the activity list and
   * the session link instead of a log (there is no log to show, and the
   * page must never imply event kinds are command output). */
  is_session?: boolean;
  /** Session jobs: the temp session doing the install (deep-link target). */
  session_id?: string | null;
  /** Session jobs: tool-level activity lines derived from the slim event
   * tail — kinds and tool names only, never payloads. */
  activity?: string[];
  /** Session jobs: one-sentence outcome/progress note. */
  message?: string | null;
}

export function jobView(job: JobRecord, logTail: string): JobView {
  const verb = job.action === "install" ? "Install" : "Remove";
  let label: string;
  let tone: JobView["tone"];
  if (job.status === "running") {
    label = job.action === "install" ? "Installing…" : "Removing…";
    tone = "busy";
  } else if (job.status === "succeeded") {
    label = `${verb} succeeded`;
    tone = "ok";
  } else {
    const code = job.exit_code;
    label =
      typeof code === "number"
        ? `${verb} failed (exit ${code})`
        : `${verb} failed`;
    tone = "bad";
  }
  if (job.kind === "session") {
    if (job.status === "running") {
      label = job.question_open
        ? "Waiting for your answer in the install session…"
        : "Installing via AI session…";
    } else if (job.status === "succeeded") {
      label = "Installed via AI session";
    } else {
      label = "Install session failed";
    }
    return {
      id: job.id,
      action: job.action,
      status: job.status,
      label,
      tone,
      exit_code: null,
      log_tail: "",
      is_session: true,
      session_id: job.session_id ?? null,
      activity: job.activity ?? [],
      message: job.message ?? null,
    };
  }
  return {
    id: job.id,
    action: job.action,
    status: job.status,
    label,
    tone,
    exit_code: job.exit_code ?? null,
    log_tail: logTail,
  };
}

// --- errors -----------------------------------------------------------------

/**
 * Turn whatever a failing exec / SSH / plugin call produced into one readable
 * sentence. The page never renders raw JSON or a bare stderr dump: every error
 * path goes through here first.
 */
export function friendlyError(raw: unknown): string {
  let msg = typeof raw === "string" ? raw : raw == null ? "" : String(raw);
  msg = unwrapJson(msg).trim();
  if (!msg) return "Something went wrong, and the target gave no reason.";

  const low = msg.toLowerCase();
  if (
    low.includes("askpass") ||
    low.includes("a password is required") ||
    low.includes("no tty present") ||
    low.includes("sudo:")
  ) {
    return (
      "This step needs root on the target, but sudo could not get a password " +
      "(Peckboard's askpass bridge is not wired into plugin commands). Run the " +
      "install manually, or give the target's user passwordless sudo. Details: " +
      msg
    );
  }
  if (low.includes("/etc/os-release")) {
    return (
      "Peckboard could not identify this target as a Linux host — it could not " +
      "read /etc/os-release. Only Linux targets can be managed here."
    );
  }
  if (
    low.includes("connection refused") ||
    low.includes("no route to host") ||
    low.includes("timed out") ||
    low.includes("timeout") ||
    low.includes("name or service not known") ||
    low.includes("could not resolve")
  ) {
    return `Could not reach the target over SSH: ${msg}`;
  }
  if (low.includes("authentication") || low.includes("permission denied")) {
    return `The target refused the SSH credentials: ${msg}`;
  }
  if (low.includes("key_id") || low.includes("ssh key vault")) {
    return `SSH key problem: ${msg}`;
  }
  // Already a plain sentence from our own code — capitalise and pass through.
  return msg.charAt(0).toUpperCase() + msg.slice(1);
}

/** If the message is (or wraps) a JSON error envelope, pull the message out. */
function unwrapJson(msg: string): string {
  const trimmed = msg.trim();
  if (!trimmed.startsWith("{")) return msg;
  try {
    const parsed = JSON.parse(trimmed);
    const inner = parsed?.error ?? parsed?.message;
    if (typeof inner === "string" && inner) return inner;
  } catch {
    /* not JSON after all — fall through and show it as-is */
  }
  return msg;
}
