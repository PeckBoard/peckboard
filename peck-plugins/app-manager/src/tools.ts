// MCP tool handlers. Every handler validates its app/target arguments against
// the catalog, the manually added apps (customApps.ts) and the target registry
// BEFORE building any shell script. A catalog app only ever runs its own
// static recipe; a manually added app runs either the AI install session
// (local) or the install/remove command the person typed for it (remote) —
// stored verbatim, never assembled from other fields.

import { CatalogApp, installRecipeFor, removeRecipeFor } from "./catalog";
import { allApps, findAnyApp } from "./customApps";
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
import {
  getDefaultInstallModel,
  pollSessionJob,
  startSessionInstall,
} from "./installSession";
import { depsOverview, refreshAfterJob } from "./deps";
import {
  getInstallRecord,
  listInstallRecords,
  settleJobProvenance,
  trackingState,
  withSnapshotBracket,
} from "./provenance";
import { TargetRecord, listTargets, resolveTarget } from "./targets";
import { appRowView, distroView, jobView, targetView } from "./view";

const PROBE_TIMEOUT_SECS = 15;
const LAUNCH_TIMEOUT_SECS = 30;

function reqStr(v: unknown, name: string): string {
  if (typeof v !== "string" || !v.trim())
    throw new Error(`${name} is required`);
  return v.trim();
}

/** Resolve an app id against the catalog, then the manually added apps. */
function resolveApp(id: string): CatalogApp {
  const app = findAnyApp(id);
  if (!app) {
    throw new Error(
      `unknown app '${id}'; not in the catalog and not one of the manually added apps`,
    );
  }
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
      ? args.apps.map((id: unknown) => resolveApp(String(id)))
      : allApps();

  const results = targets.map((target: TargetRecord) => {
    let packageManager: PackageManager | null = null;
    try {
      packageManager = probeDistro(target).pm;
    } catch {
      packageManager = null;
    }
    const records = listInstallRecords(target.id);
    const scoped = apps.map((app: CatalogApp) => ({
      app,
      record: records.find((r) => r.app_id === app.id) ?? null,
    }));
    return {
      target: { id: target.id, label: target.label },
      package_manager: packageManager,
      apps: scoped.map(({ app, record }) => ({
        id: app.id,
        name: app.name,
        namespace: app.namespace ?? "system",
        // Manually added apps are named as such on the tool surface too: an
        // agent reading this must not mistake one for a vetted catalog entry.
        ...(app.custom ? { source: "manual" } : { source: "catalog" }),
        ...probeApp(target, app),
        ...(record?.primary ? { package_version: record.primary.version } : {}),
        ...(record ? { package_tracking: trackingState(record) } : {}),
      })),
      // Provenance, not dependencies: each entry arrived during the labelled
      // app's install job (snapshot-bracket delta), with its package-DB
      // version — see provenance.ts.
      added_packages: scoped.flatMap(({ app, record }) =>
        (record?.added ?? []).map((p) => ({
          name: p.name,
          version: p.version,
          installed_with: app.id,
        })),
      ),
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
      const wasRunning = job.status === "running";
      const polled =
        job.kind === "session"
          ? pollSessionJob(target, job)
          : pollJob(target, job);
      job = polled.job;
      tail = polled.tail;
      if (wasRunning && job.status !== "running") {
        // First observation of the terminal state: consume the snapshot
        // bracket into an install record (or drop the record on a remove).
        try {
          settleJobProvenance(target, job);
        } catch {
          /* provenance must never break status reporting */
        }
        // A settled job changed the installed set, so the cached dependency
        // graph is stale. Swallows its own failures (see deps.ts).
        refreshAfterJob(target, job);
      }
    } catch (e) {
      tail = `(could not poll job status: ${errMsg(e)})`;
    }
  }
  return { probe, job, tail };
}

export function appStatus(args: any): any {
  const target = resolveTarget(reqStr(args?.target, "target"));
  const app = resolveApp(reqStr(args?.app, "app"));
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
          ...(job.kind === "session"
            ? {
                kind: "session",
                session_id: job.session_id ?? null,
                activity: job.activity ?? [],
                ...(job.message ? { message: job.message } : {}),
              }
            : {}),
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
  meta: Pick<JobRecord, "pm" | "method"> = {},
): any {
  const job = createJob(target.id, app.id, action, meta);
  try {
    // Installs are bracketed by package-database snapshots inside the same
    // detached script, so the delta covers exactly the install's lifetime
    // — see provenance.ts.
    const command =
      action === "install"
        ? withSnapshotBracket(recipe, meta.pm ?? null, job.id)
        : recipe;
    const script = buildBackgroundScript(command, job.logfile);
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
  const app = resolveApp(reqStr(args?.app, "app"));
  const distro = probeDistro(target);
  const recipe = installRecipeFor(app, distro.pm);
  // A manually added app has no authored recipe. On the local host that's
  // fine — the AI session works the method out (and is held to official
  // sources, see installSession.ts). On a remote target there is no session
  // to do that, so the person's own install command is the only way in.
  if (!recipe && !(app.custom && target.kind === "local")) {
    throw new Error(
      app.custom
        ? `no install command for '${app.id}' on target '${target.id}': manually added apps are worked out by ` +
            `an AI install session, and that session only runs on the local Peckboard host. Either give this app an ` +
            `install command in the App Manager dashboard, or install it on the local host.`
        : `no install method for '${app.id}' on target '${target.id}': unsupported/unrecognised Linux ` +
            `distribution ${distroDescription(distro)} (supported: debian/ubuntu, fedora/rhel, arch, suse) ` +
            `and no vendor script configured for this app`,
    );
  }

  // LOCAL installs run through a TEMPORARY AI SESSION on a user-picked
  // account + model (see installSession.ts). Remote targets keep the
  // deterministic script path: an AI session runs on the Peckboard host and
  // has no path to a remote target's SSH credentials. Removal stays
  // script-based everywhere by design.
  if (target.kind === "local") {
    const model =
      typeof args?.model === "string" && args.model.trim()
        ? args.model.trim()
        : getDefaultInstallModel();
    if (!model) {
      throw new Error(
        `installing on the local host runs a temporary AI session; pick the account and model for it ` +
          `(pass 'model', or choose one in the App Manager dashboard once to set the default)`,
      );
    }
    return startSessionInstall(target, app, model, distro.pm);
  }

  // Only a manually added app on the local target reaches here without a
  // recipe, and that returned above through the session path.
  if (!recipe) {
    throw new Error(
      `no install command for '${app.id}' on target '${target.id}'`,
    );
  }

  // pip-namespace installs run without the package-DB snapshot bracket
  // (pm: null): pip's packages are invisible to it, and an unrelated
  // background distro change must not get attributed to the pip app.
  const method: NonNullable<JobRecord["method"]> = app.install.pip
    ? "pip"
    : distro.pm && app.install[distro.pm]
      ? distro.pm
      : "vendor";
  return startJob(target, app, "install", recipe, {
    pm: method === "pip" ? null : distro.pm,
    method,
  });
}

export function appRemove(args: any): any {
  const target = resolveTarget(reqStr(args?.target, "target"));
  const app = resolveApp(reqStr(args?.app, "app"));
  const distro = probeDistro(target);
  const recipe = removeRecipeFor(app, distro.pm);
  if (!recipe) {
    throw new Error(
      app.custom
        ? `no remove command for '${app.id}': App Manager never guesses how to uninstall a manually added ` +
            `app. Give this app a remove command in the dashboard, or use Forget to drop it from the list ` +
            `(which uninstalls nothing).`
        : `no remove method for '${app.id}' on target '${target.id}': unsupported/unrecognised Linux ` +
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
 * isn't a usable Linux target), and one row per app — catalog first, then the
 * manually added ones — with its installed state plus any job attached to it.
 * Job records come from the data store, so this costs no extra exec beyond
 * the per-app probes.
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
    ? allApps().map((app) =>
        appRowView(
          app,
          probeApp(target, app),
          probe ? probe.pm : null,
          currentJobFor(target.id, app.id),
          getInstallRecord(target.id, app.id),
          target.kind,
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
  const app = resolveApp(reqStr(appRef, "app"));
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
    row: appRowView(
      app,
      probe,
      pm,
      job,
      getInstallRecord(target.id, app.id),
      target.kind,
    ),
    job: job ? jobView(job, tail) : null,
  };
}

/** The list behind the target dropdown. */
export function targetChoices(): any {
  return { targets: listTargets().map(targetView) };
}

// --- app_deps ---------------------------------------------------------------

/** The cached dependency graph for one target: per-app trees, the reverse
 * (library → apps) view, and removal impact. Read-only — resolution happens
 * on job settle and explicit refresh (see deps.ts), never here. */
export function appDeps(args: any): any {
  const target = resolveTarget(reqStr(args?.target, "target"));
  return depsOverview(target);
}
