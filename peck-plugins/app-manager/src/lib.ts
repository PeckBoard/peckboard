// Hook dispatch. Tool calls are short, bounded execs (or a fast job-launch /
// poll), so unlike ssh-fleet's long-lived SSH sessions there's no need for the
// defer/finalize round-trip; each tool call resolves synchronously within one
// invocation. The two http.* hooks serve the dashboard page and its
// authenticated data routes (see http.ts).

import {
  appDeps,
  appInstall,
  appList,
  appRemove,
  appStatus,
  appTargets,
} from "./tools";
import { serveAuthed, serveHttp } from "./http";
import { allow, cancel, errMsg, skip } from "./verdict";

const TOOLS: Record<string, (args: any) => any> = {
  app_targets: appTargets,
  app_list: appList,
  app_status: appStatus,
  app_install: appInstall,
  app_remove: appRemove,
  app_deps: appDeps,
};

function handleInvoke(payload: any): string {
  if (
    payload === null ||
    payload === undefined ||
    typeof payload !== "object"
  ) {
    return cancel("malformed invoke payload: not an object");
  }
  const tool: string = typeof payload.tool === "string" ? payload.tool : "";
  const args = payload.arguments ?? {};

  const fn = TOOLS[tool];
  if (!fn) return cancel(`app-manager does not provide tool '${tool}'`);

  try {
    return allow(fn(args));
  } catch (e) {
    // A handler error is a normal tool result (the agent sees the message),
    // not a plugin cancel.
    return allow({ error: errMsg(e) });
  }
}

export function dispatch(hook: string, payload: any): string {
  switch (hook) {
    case "mcp.tool.invoke":
      return handleInvoke(payload);
    case "http.request.before":
      return serveHttp(payload);
    case "http.request.authed":
      return serveAuthed(payload);
    default:
      return skip();
  }
}
