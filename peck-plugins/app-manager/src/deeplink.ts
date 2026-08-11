// Deep-link prefill for the dashboard page.
//
// Another plugin's page can link a person here with what it needs installed —
// graphify's install handoff opens
// `/plugin-page/app-manager/app-manager?install=python3,pip,graphifyy&from=graphify`.
//
// The link ONLY PREFILLS. It names apps in a request bar above the grid; a
// person still clicks Install, and on a local target still picks the account
// and model for the install session. No URL can start an install, and nothing
// here bypasses a confirmation — plugins stay isolated, so this is a handoff
// between two pages a user drives, not one plugin invoking another.
//
// Parsing happens here (server side, unit-tested) rather than in the page's
// browser JS, and the result is injected into the page as a JSON literal.

import { queryParam } from "./query";

/// What a deep link asked for. `apps` are catalog ids the page resolves
/// against the current target's overview; unknown ids are reported, never
/// acted on.
export interface InstallRequest {
  apps: string[];
  from: string;
  target: string;
}

/// The token page.ts carries where the request literal is injected.
export const REQUEST_TOKEN = "__APP_MANAGER_REQUEST__";

// Catalog ids and target ids are lowercase slugs; anything else is dropped
// rather than echoed into the page.
const ID_RE = /^[a-z0-9][a-z0-9._+-]{0,63}$/;
const MAX_APPS = 12;
const FROM_MAX = 40;

/// Parse `?install=a,b&from=X&target=T`. Returns null when nothing usable was
/// asked for, so the page hides the request bar entirely.
export function parseInstallRequest(query: string): InstallRequest | null {
  const raw = queryParam(query, "install");
  if (!raw) return null;

  const apps: string[] = [];
  for (const part of raw.split(",")) {
    const id = part.trim().toLowerCase();
    if (!ID_RE.test(id) || apps.indexOf(id) >= 0) continue;
    apps.push(id);
    if (apps.length >= MAX_APPS) break;
  }
  if (!apps.length) return null;

  const target = (queryParam(query, "target") || "").trim().toLowerCase();
  return {
    apps,
    from: sanitizeFrom(queryParam(query, "from")),
    target: ID_RE.test(target) ? target : "",
  };
}

/// `from` is displayed text, not an id: keep it to plain words so a link can't
/// smuggle markup or a novel-length label into the bar.
export function sanitizeFrom(value: string | undefined): string {
  if (!value) return "";
  return value
    .replace(/[^A-Za-z0-9 ._-]/g, "")
    .trim()
    .slice(0, FROM_MAX);
}

/// Serialize the request for injection into a <script> block. `<`, `>` and
/// `&` are escaped so the literal can never close the script element early,
/// and U+2028/U+2029 because they are legal raw in JSON but are line
/// terminators in JavaScript.
export function requestLiteral(req: InstallRequest | null): string {
  let out = "";
  for (const ch of JSON.stringify(req)) {
    const code = ch.charCodeAt(0);
    if (
      ch === "<" ||
      ch === ">" ||
      ch === "&" ||
      code === 0x2028 ||
      code === 0x2029
    ) {
      out += "\\u" + code.toString(16).padStart(4, "0");
      continue;
    }
    out += ch;
  }
  return out;
}

/// Bake the parsed request into the page HTML.
export function injectRequest(
  page: string,
  req: InstallRequest | null,
): string {
  return page.replace(REQUEST_TOKEN, requestLiteral(req));
  // Function replacement: `$` sequences in a string replacement would be
  // treated as substitution patterns.
  const literal = requestLiteral(req);
  return page.replace(REQUEST_TOKEN, () => literal);
}
