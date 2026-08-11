// Plugin manifest — see peckboard/src/plugin/hooks.rs (PluginManifest) and
// src/plugin/manager.rs (ALLOWED_PERMISSIONS/ALLOWED_HOOKS, mcp_tools
// validation) in the core repo for what's required/validated on load.

const DESCRIPTION =
  "Lists, installs, and removes common applications (git, Claude Code, cursor-agent, " +
  "Ollama, Node.js, Docker, ripgrep, Python 3, pip) on Linux targets — the local " +
  "Peckboard host and any configured remote SSH hosts — plus pip-namespace Python " +
  "packages (graphifyy), which are labelled distinctly: pip's namespace is separate " +
  "from the distro package database. Installs and removals run real package-manager " +
  "commands (apt/dnf/pacman/zypper), vendor installer scripts, or pip AS THE PECKBOARD " +
  "HOST USER on the chosen target, using sudo -A for steps that need root. This plugin " +
  "can run any bare command on the local host via the process_exec_any permission — it " +
  "is restricted in code to the app catalog's own static recipes plus whatever install/" +
  "remove command you type for an app you add by hand, but the permission " +
  "picking a target, seeing what is installed, watching install progress live, and " +
  "browsing each app's dependency graph as resolved from the target's package manager " +
  "(pip packages get their own pip-probed section). Installs on the LOCAL host run " +
  "through a TEMPORARY AI SESSION on a user-picked account + model (thinking-capable " +
  "models only): the plugin creates the temp session, dispatches the install prompt, " +
  "shows tool-level session activity, and still records provenance itself via " +
  "package-DB snapshots taken around the session — removal stays script-based. Apps " +
  "outside the catalog can be ADDED BY HAND in the dashboard: on the local host that " +
  "install session identifies the software (searching the web when it does not know it) " +
  "and downloads only from official sources; on a remote target it runs the install " +
  "command the person typed for that app — user-authored shell run verbatim on the " +
  "target, the one thing this plugin runs that is not a static catalog recipe.";

const VERSION = "0.7.0";
const REPOSITORY = "https://github.com/PeckBoard/app-manager";

// Inline SVG (lucide "package") for the sidebar entry; rendered sandboxed.
const ICON =
  '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" ' +
  'stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">' +
  '<path d="m7.5 4.27 9 5.15"/>' +
  '<path d="M21 8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16Z"/>' +
  '<path d="m3.3 7 8.7 5 8.7-5"/><path d="M12 22V12"/></svg>';

const TARGET_DESC =
  "Target id from app_targets (e.g. 'local', or a configured remote target's id).";
const APP_DESC =
  "App id from app_list (a catalog id such as 'git', 'ollama', 'ripgrep', or the id of an " +
  "app added by hand in the App Manager dashboard).";
const MCP_TOOLS = [
  {
    name: "app_targets",
    description:
      "List configured targets: the local Peckboard host plus any configured remote SSH hosts.",
    input_schema: {
      type: "object",
      properties: {},
      additionalProperties: false,
    },
  },
  {
    name: "app_list",
    description:
      "List the app catalog plus any apps added by hand in the dashboard (each entry carries " +
      "source: 'catalog' or 'manual'), with per-target installed state and version, plus the " +
      "packages each recorded install genuinely added (snapshot-bracket provenance: name + " +
      "package-DB version, labelled with the app that pulled them in; vendor-script installs are " +
      "explicitly untracked by the package manager). Optionally scope to a subset of targets " +
      "and/or apps; defaults to every configured target and every app.",
    input_schema: {
      type: "object",
      properties: {
        targets: {
          type: "array",
          items: { type: "string" },
          description: "Target ids to check (default: all configured targets).",
        },
        apps: {
          type: "array",
          items: { type: "string" },
          description:
            "App ids to check (default: the catalog plus every manually added app).",
        },
      },
      required: [],
      additionalProperties: false,
    },
  },
  {
    name: "app_status",
    description:
      "Status of one app on one target: whether it's installed, its version, and the state of any " +
      "in-flight (or most recent) install/remove job for it, including a log tail.",
    input_schema: {
      type: "object",
      properties: {
        app: { type: "string", description: APP_DESC },
        target: { type: "string", description: TARGET_DESC },
      },
      required: ["app", "target"],
      additionalProperties: false,
    },
  },
  {
    name: "app_install",
    description:
      "Start installing an app on a target. Returns immediately with a job id — poll app_status " +
      "with the same app/target to follow progress. On the LOCAL host the install runs through a " +
      "temporary AI session on a thinking-capable model: pass 'model' (an id from the dashboard's " +
      "picker, account-qualified) or rely on the dashboard's stored default; without either the " +
      "call is refused. Remote targets install via the deterministic scripted recipe and take no " +
      "model. A manually added app has no catalog recipe: on the local host the session works out " +
      "how to install it (identifying the software from official sources), and on a remote target " +
      "it needs the install command stored with that app in the dashboard, or the call is refused.",
    input_schema: {
      type: "object",
      properties: {
        app: { type: "string", description: APP_DESC },
        target: { type: "string", description: TARGET_DESC },
        model: {
          type: "string",
          description:
            "Local installs only: the account-qualified model id the temporary install session " +
            "runs on (thinking-capable models only; the server validates against its own catalog). " +
            "Defaults to the last model chosen in the App Manager dashboard.",
        },
      },
      required: ["app", "target"],
      additionalProperties: false,
    },
  },
  {
    name: "app_remove",
    description:
      "Start removing an app from a target. Returns immediately with a job id — poll app_status with " +
      "the same app/target to follow progress. Removal is always a deterministic scripted command: a " +
      "catalog app's own remove recipe, or the remove command stored with a manually added app — " +
      "never a guess, so an app with neither cannot be removed here.",
    input_schema: {
      type: "object",
      properties: {
        app: { type: "string", description: APP_DESC },
        target: { type: "string", description: TARGET_DESC },
      },
      required: ["app", "target"],
      additionalProperties: false,
    },
  },
  {
    name: "app_deps",
    description:
      "The cached dependency graph for one target, resolved from its package manager " +
      "(apt/rpm/pacman): per-app dependency trees (name + version + app/library/binary kind, " +
      "shared multi-parent nodes flagged), the reverse view (which apps require a library), " +
      "and removal impact honouring autoremove semantics (a dependency still required by " +
      "another app is never listed as collateral). Read-only — the graph refreshes when an " +
      "install/remove job settles or on explicit refresh from the dashboard. Vendor-script " +
      "installs are explicitly not tracked by the package manager.",
    input_schema: {
      type: "object",
      properties: {
        target: { type: "string", description: TARGET_DESC },
      },
      required: ["target"],
      additionalProperties: false,
    },
  },
];

export function manifestJson(): string {
  return JSON.stringify({
    description: DESCRIPTION,
    version: VERSION,
    repository: REPOSITORY,
    // app_list can chain many short probes across several targets; give it
    // headroom well under core's MAX_CALL_TIMEOUT (610s) rather than tripping
    // the Extism default ~2s budget on the first real call.
    call_timeout_secs: 300,
    hooks: ["mcp.tool.invoke", "http.request.before", "http.request.authed"],
    permissions: [
      "provide_mcp_tools",
      "data_store",
      "process_exec_any",
      "ssh",
      "ssh_keys",
      "user_authority", // serve the authenticated dashboard data routes
      "contribute_sidebar", // the App Manager sidebar page
      "models_read", // the install picker's account+model catalog (metadata only)
      "session_write", // create the temporary install session
      "session_dispatch", // dispatch the install prompt at it
      "session_read", // poll its slim event tail for progress
    ],

    // Global sidebar entry → the app-manager dashboard.
    sidebar_items: [
      {
        id: "app-manager",
        label: "App Manager",
        icon: ICON,
        path: "/plugin-api/v1/app-manager",
      },
    ],

    http_routes: ["GET /plugin-api/v1/app-manager"],

    ui_routes: [
      "GET /api/plugin-ui/app-manager/targets",
      "GET /api/plugin-ui/app-manager/ssh-keys",
      "GET /api/plugin-ui/app-manager/apps",
      "GET /api/plugin-ui/app-manager/install-options",
      "GET /api/plugin-ui/app-manager/status",
      "GET /api/plugin-ui/app-manager/deps",
      "GET /api/plugin-ui/app-manager/rdeps",
      "POST /api/plugin-ui/app-manager/targets",
      "POST /api/plugin-ui/app-manager/target-remove",
      "POST /api/plugin-ui/app-manager/install",
      "POST /api/plugin-ui/app-manager/remove",
      "GET /api/plugin-ui/app-manager/apps-custom",
      "POST /api/plugin-ui/app-manager/apps-custom",
      "POST /api/plugin-ui/app-manager/apps-custom-remove",
      "POST /api/plugin-ui/app-manager/deps-refresh",
    ],
    mcp_tools: MCP_TOOLS,
  });
}
