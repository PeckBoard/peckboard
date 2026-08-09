// MCP tool handlers. Every handler validates its app/target arguments
// against the catalog and target registry BEFORE building any shell script —
// nothing here ever interpolates free-form user text into a command; only
// the catalog's own static recipes (see catalog.ts) run on a target.

import {
  APPS,
  CatalogApp,
  findApp,
  installRecipeFor,
  removeRecipeFor,
} from "./catalog";
import {
  OS_RELEASE_PROBE,
  PackageManager,
  detectPackageManager,
  parseOsRelease,
} from "./distro";
import { errMsg } from "./verdict";
import { runOnTarget } from "./exec";
import {
  JobAction,
  JobRecord,
  buildBackgroundScript,
  createJob,
  currentJobFor,
  pollJob,
  putJob,
} from "./jobs";
import { TargetRecord, listTargets, resolveTarget } from "./targets";
import { appRowView, distroView, jobView, targetView } from "./view";

const PROBE_TIMEOUT_SECS = 15;
const LAUNCH_TIMEOUT_SECS = 30;

function reqStr(v: unknown, name: string): string {
  if (typeof v !== "string" || !v.trim())
    throw new Error(`${name} is required`);
  return v.trim();
}

function resolveCatalogApp(id: string): CatalogApp {
  const app = findApp(id);
  if (!app) throw new Error(`unknown app '${id}'; not in the catalog`);
  return app;
}

interface DistroInfo {
  pm: PackageManager | null;
  id: string;
  idLike: string[];
}

/** Confirm the target is Linux (refuses clearly if not) and map its distro
 * to a package manager, or null if unrecognised. Never guesses. */
function probeDistro(target: TargetRecord): DistroInfo {
  const res = runOnTarget(target, OS_RELEASE_PROBE, PROBE_TIMEOUT_SECS);
  if (!res.ok || !res.stdout.trim()) {
    throw new Error(
      `could not read /etc/os-release on target '${target.id}'; this plugin only supports Linux targets`,
    );
  }
  const release = parseOsRelease(res.stdout);
  return {
    pm: detectPackageManager(release),
    id: release.id,
    idLike: release.idLike,
  };
}

function distroDescription(d: DistroInfo): string {
  if (!d.id) return "unrecognised";
  return d.idLike.length
    ? `'${d.id}' (like: ${d.idLike.join(", ")})`
    : `'${d.id}'`;
}

function probeApp(
  target: TargetRecord,
  app: CatalogApp,
): { installed: boolean; version: string | null; error?: string } {
  try {
    const detect = runOnTarget(target, app.detect, PROBE_TIMEOUT_SECS);
    if (!detect.ok) return { installed: false, version: null };
    const version = runOnTarget(target, app.version, PROBE_TIMEOUT_SECS);
    const line = version.ok
      ? version.stdout.trim().split("\n")[0] || null
      : null;
    return { installed: true, version: line };
  } catch (e) {
    return { installed: false, version: null, error: errMsg(e) };
  }
}

// --- app_targets ------------------------------------------------------------

export function appTargets(_args: any): any {
  const targets = listTargets().map((t) => ({
    id: t.id,
    kind: t.kind,
    label: t.label,
    hostname: t.hostname,
    port: t.port,
    username: t.username,
  }));
  return { targets };
}

// --- app_list -----------------------------------------------------------------

export function appList(args: any): any {
  const targets =
    Array.isArray(args?.targets) && args.targets.length
      ? args.targets.map((t: unknown) => resolveTarget(t))
      : listTargets();
  const apps =
    Array.isArray(args?.apps) && args.apps.length
      ? args.apps.map((id: unknown) => resolveCatalogApp(String(id)))
      : APPS;

  const results = targets.map((target: TargetRecord) => {
    let packageManager: PackageManager | null = null;
    try {
      packageManager = probeDistro(target).pm;
    } catch {
      packageManager = null;
    }
    return {
      target: { id: target.id, label: target.label },
      package_manager: packageManager,
      apps: apps.map((app: CatalogApp) => ({
        id: app.id,
        name: app.name,
        ...probeApp(target, app),
      })),
    };
  });

  return { targets: results };
}

// --- app_status -----------------------------------------------------------------

/** The shared read path behind app_status and the UI's /status route: probe
 * the app, then poll whatever job is (or was last) attached to it. */
function appState(
  target: TargetRecord,
  app: CatalogApp,
): {
  probe: { installed: boolean; version: string | null; error?: string };
  job: JobRecord | null;
  tail: string;
} {
  const probe = probeApp(target, app);
  let job = currentJobFor(target.id, app.id);
  let tail = "";
  if (job) {
    try {
      const polled = pollJob(target, job);
      job = polled.job;
      tail = polled.tail;
    } catch (e) {
      tail = `(could not poll job status: ${errMsg(e)})`;
    }
  }
  return { probe, job, tail };
}

export function appStatus(args: any): any {
  const target = resolveTarget(reqStr(args?.target, "target"));
  const app = resolveCatalogApp(reqStr(args?.app, "app"));
  const { probe, job, tail } = appState(target, app);

  return {
    app: app.id,
    target: target.id,
    installed: probe.installed,
    version: probe.version,
    ...(probe.error ? { probe_error: probe.error } : {}),
    job: job
      ? {
          id: job.id,
          action: job.action,
          status: job.status,
          exit_code: job.exit_code ?? null,
          log_tail: tail,
        }
      : null,
  };
}

// --- app_install / app_remove -----------------------------------------------

function startJob(
  target: TargetRecord,
  app: CatalogApp,
  action: JobAction,
  recipe: string,
): any {
  const job = createJob(target.id, app.id, action);
  try {
    const script = buildBackgroundScript(recipe, job.logfile);
    const res = runOnTarget(target, script, LAUNCH_TIMEOUT_SECS);
    if (!res.ok) {
      throw new Error(
        res.stderr.trim() || res.stdout.trim() || "failed to launch the job",
      );
    }
    const pid = parseInt(res.stdout.trim(), 10);
    if (!Number.isFinite(pid) || pid <= 0) {
      throw new Error(
        `could not determine the process id for the started ${action} job`,
      );
    }
    putJob({ ...job, pid });
    return {
      job_id: job.id,
      app: app.id,
      target: target.id,
      action,
      status: "running",
    };
  } catch (e) {
    putJob({ ...job, status: "failed" });
    throw new Error(
      `failed to start ${action} for '${app.id}' on '${target.id}': ${errMsg(e)}`,
    );
  }
}

export function appInstall(args: any): any {
  const target = resolveTarget(reqStr(args?.target, "target"));
  const app = resolveCatalogApp(reqStr(args?.app, "app"));
  const distro = probeDistro(target);
  const recipe = installRecipeFor(app, distro.pm);
  if (!recipe) {
    throw new Error(
      `no install method for '${app.id}' on target '${target.id}': unsupported/unrecognised Linux ` +
        `distribution ${distroDescription(distro)} (supported: debian/ubuntu, fedora/rhel, arch, suse) ` +
        `and no vendor script configured for this app`,
    );
  }
  return startJob(target, app, "install", recipe);
}

export function appRemove(args: any): any {
  const target = resolveTarget(reqStr(args?.target, "target"));
  const app = resolveCatalogApp(reqStr(args?.app, "app"));
  const distro = probeDistro(target);
  const recipe = removeRecipeFor(app, distro.pm);
  if (!recipe) {
    throw new Error(
      `no remove method for '${app.id}' on target '${target.id}': unsupported/unrecognised Linux ` +
        `distribution ${distroDescription(distro)} (supported: debian/ubuntu, fedora/rhel, arch, suse) ` +
        `and no vendor script configured for this app`,
    );
  }
  return startJob(target, app, "remove", recipe);
}

// --- UI surface (see http.ts / page.ts) -------------------------------------

/**
 * Everything the dashboard needs for ONE target in a single round trip: the
 * target itself, its distro/package-manager banner (or the refusal when it
 * isn't a usable Linux target), and one row per catalog app with its installed
 * state plus any job attached to it. Job records come from the data store, so
 * this costs no extra exec beyond the per-app probes.
 */
export function targetOverview(targetRef: unknown): any {
  const target = resolveTarget(targetRef);
  let probe: DistroInfo | null = null;
  let probeError: string | null = null;
  try {
    probe = probeDistro(target);
  } catch (e) {
    probeError = errMsg(e);
  }
  const distro = distroView(probe, probeError);
  const apps = distro.supported
    ? APPS.map((app) =>
        appRowView(
          app,
          probeApp(target, app),
          probe ? probe.pm : null,
          currentJobFor(target.id, app.id),
        ),
      )
    : [];
  return { target: targetView(target), distro, apps };
}

/**
 * The dashboard's install-progress poll: one app's live state plus the job's
 * log tail. Returns the SAME row shape as `targetOverview` so the page can
 * swap a row wholesale instead of patching fields (and drifting from what the
 * server would say). The distro re-probe is a `cat /etc/os-release`, so it
 * costs nothing next to the app probe it accompanies.
 */
export function appProgress(targetRef: unknown, appRef: unknown): any {
  const target = resolveTarget(targetRef);
  const app = resolveCatalogApp(reqStr(appRef, "app"));
  let pm: PackageManager | null = null;
  try {
    pm = probeDistro(target).pm;
  } catch {
    pm = null;
  }
  const { probe, job, tail } = appState(target, app);
  return {
    app: app.id,
    target: target.id,
    row: appRowView(app, probe, pm, job),
    job: job ? jobView(job, tail) : null,
  };
}

/** The list behind the target dropdown. */
export function targetChoices(): any {
  return { targets: listTargets().map(targetView) };
}
