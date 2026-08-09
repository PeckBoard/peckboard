// Plugin manifest — see peckboard/src/plugin/hooks.rs (PluginManifest) and
// src/plugin/manager.rs (ALLOWED_PERMISSIONS/ALLOWED_HOOKS, mcp_tools
// validation) in the core repo for what's required/validated on load.

const DESCRIPTION =
  "Lists, installs, and removes common applications (git, Claude Code, cursor-agent, " +
  "Ollama, Node.js, Docker, ripgrep) on Linux targets — the local Peckboard host and " +
  "any configured remote SSH hosts. Installs and removals run real package-manager " +
  "commands (apt/dnf/pacman/zypper) or vendor installer scripts AS THE PECKBOARD HOST " +
  "USER on the chosen target, using sudo -A for steps that need root. This plugin can " +
  "run any bare command on the local host via the process_exec_any permission — it is " +
  "restricted in code to the app catalog's own static recipes, but the permission grant " +
  "itself is broad. Ships an App Manager dashboard page (sidebar entry) for picking a " +
  "target, seeing what is installed, and watching install progress live.";

const VERSION = "0.3.0";
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
  "Catalog app id from app_list (e.g. 'git', 'ollama', 'ripgrep').";

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
      "List the app catalog with per-target installed state and version, plus the packages each " +
      "recorded install genuinely added (snapshot-bracket provenance: name + package-DB version, " +
      "labelled with the app that pulled them in; vendor-script installs are explicitly untracked " +
      "by the package manager). Optionally scope to a subset of targets and/or apps; defaults to " +
      "every configured target and every catalog app.",
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
          description: "Catalog app ids to check (default: the whole catalog).",
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
      "Start installing an app on a target. Returns immediately with a job id — installs run detached " +
      "and can take minutes; poll app_status with the same app/target to follow progress.",
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
    name: "app_remove",
    description:
      "Start removing an app from a target. Returns immediately with a job id — poll app_status with " +
      "the same app/target to follow progress. Only apps in the catalog can be removed.",
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
      "GET /api/plugin-ui/app-manager/status",
      "POST /api/plugin-ui/app-manager/targets",
      "POST /api/plugin-ui/app-manager/target-remove",
      "POST /api/plugin-ui/app-manager/install",
      "POST /api/plugin-ui/app-manager/remove",
    ],
    mcp_tools: MCP_TOOLS,
  });
}
