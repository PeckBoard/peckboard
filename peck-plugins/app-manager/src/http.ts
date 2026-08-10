// HTTP surfaces: the dashboard page itself (`http.request.before`, public,
// framed in a sandboxed iframe) and the authenticated data routes the page
// calls through the parent-proxied fetch bridge (`http.request.authed`,
// gated by the `user_authority` permission).
//
// Every error leaves here as one readable sentence (`friendlyError`) — the
// page renders `error` verbatim and never sees a raw stderr dump or a JSON
// envelope. Key material never appears on any of these routes: the key picker
// gets metadata only, and a target stores nothing but the vault key's id.

import { PAGE } from "./page";
import { listModels, sshKeyList } from "./host";
import { getDefaultInstallModel, thinkingModelChoices } from "./installSession";
import {
  appDeps,
  appInstall,
  appProgress,
  appRemove,
  targetChoices,
  targetOverview,
} from "./tools";
import { refreshDepGraph, systemReverseDeps } from "./deps";
import {
  buildRecord,
  deleteTarget,
  getTarget,
  putTarget,
  resolveTarget,
} from "./targets";
import { errMsg, htmlResponse, jsonResponse } from "./verdict";
import { friendlyError, targetView } from "./view";

const PAGE_PATH = "/plugin-api/v1/app-manager";
const API = "/api/plugin-ui/app-manager";

function up(v: unknown): string {
  return (typeof v === "string" ? v : "").toUpperCase();
}
function str(v: unknown): string {
  return typeof v === "string" ? v : "";
}
function parseBody(body: string): any {
  if (!body) return {};
  try {
    return JSON.parse(body);
  } catch (e) {
    throw new Error("invalid request body: " + errMsg(e));
  }
}

/// Serve the dashboard page (the sidebar item opens this).
export function serveHttp(payload: any): string {
  if (up(payload?.method) === "GET" && str(payload?.path) === PAGE_PATH) {
    return htmlResponse(200, PAGE);
  }
  return htmlResponse(
    404,
    "<!doctype html><title>Not found</title><p>Not found.</p>",
  );
}

/// Authenticated app-UI endpoints under /api/plugin-ui/app-manager/*.
export function serveAuthed(payload: any): string {
  const method = up(payload?.method);
  const path = str(payload?.path);
  const query = str(payload?.query);
  const body = str(payload?.body);

  try {
    if (method === "GET" && path === `${API}/targets`) {
      return jsonResponse(200, targetChoices());
    }
    // Metadata only — core never hands a plugin key material.
    if (method === "GET" && path === `${API}/ssh-keys`) {
      return jsonResponse(200, { keys: sshKeyList() });
    }
    if (method === "GET" && path === `${API}/apps`) {
      return jsonResponse(200, targetOverview(queryParam(query, "target")));
    }
    // The install picker: selectable accounts + models (thinking-capable
    // only, filtered server-side by core) plus the stored default. A fixed
    // option set for a <select> — the page never offers free-text input.
    if (method === "GET" && path === `${API}/install-options`) {
      return jsonResponse(200, {
        models: thinkingModelChoices(listModels()),
        default_model: getDefaultInstallModel(),
      });
    }
    if (method === "GET" && path === `${API}/status`) {
      return jsonResponse(
        200,
        appProgress(queryParam(query, "target"), queryParam(query, "app")),
      );
    }
    if (method === "GET" && path === `${API}/deps`) {
      return jsonResponse(
        200,
        appDeps({ target: queryParam(query, "target") }),
      );
    }
    if (method === "GET" && path === `${API}/rdeps`) {
      const target = resolveTarget(queryParam(query, "target"));
      return jsonResponse(
        200,
        systemReverseDeps(target, queryParam(query, "pkg")),
      );
    }
    if (method === "POST" && path === `${API}/deps-refresh`) {
      const b = parseBody(body);
      return jsonResponse(
        200,
        refreshDepGraph(resolveTarget(b?.target), b?.depth),
      );
    }
    if (method === "POST" && path === `${API}/targets`) {
      return jsonResponse(200, { target: saveTarget(parseBody(body)) });
    }
    if (method === "POST" && path === `${API}/target-remove`) {
      const rec = resolveTarget(parseBody(body)?.id);
      if (!deleteTarget(rec.id)) {
        throw new Error(`target '${rec.label}' cannot be removed`);
      }
      return jsonResponse(200, { removed: rec.id });
    }
    if (method === "POST" && path === `${API}/install`) {
      const b = parseBody(body);
      return jsonResponse(
        200,
        appInstall({ target: b?.target, app: b?.app, model: b?.model }),
      );
    }
    if (method === "POST" && path === `${API}/remove`) {
      const b = parseBody(body);
      return jsonResponse(200, appRemove({ target: b?.target, app: b?.app }));
    }
  } catch (e) {
    return jsonResponse(400, { error: friendlyError(errMsg(e)) });
  }
  return jsonResponse(404, { error: "Not found." });
}

/// Create or update a remote target. `id` present = edit; the record is
/// rebuilt (and revalidated) from the existing one plus the submitted fields.
function saveTarget(input: any): any {
  const id = str(input?.id).trim();
  let existing = null;
  if (id) {
    existing = getTarget(id);
    if (!existing || existing.kind !== "remote") {
      throw new Error(`unknown remote target '${id}'`);
    }
  }
  const rec = buildRecord(input, existing);
  putTarget(rec);
  return targetView(rec);
}

/// Extract and URL-decode `name`'s value from a `&`-separated query string.
export function queryParam(query: string, name: string): string | undefined {
  for (const pair of query.split("&")) {
    const idx = pair.indexOf("=");
    if (idx < 0) continue;
    if (pair.slice(0, idx) !== name) continue;
    const v = pair.slice(idx + 1);
    try {
      return decodeURIComponent(v.replace(/\+/g, "%20"));
    } catch (_e) {
      return v;
    }
  }
  return undefined;
}
