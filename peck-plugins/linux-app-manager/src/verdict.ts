// Response envelope helpers for the mcp.tool.invoke and http.* hooks. Pure —
// no Extism runtime dependency, so these load under vitest without a
// Host/Memory shim.

export function skip(): string {
  return JSON.stringify({ verdict: "skip" });
}

export function allow(value: unknown): string {
  return JSON.stringify({ verdict: "allow", payload: value });
}

export function cancel(reason: string): string {
  return JSON.stringify({ verdict: "cancel", reason });
}

/// Wrap a JSON value as a `Verdict::Allow` HTTP response (the shape the
/// http.request.before / http.request.authed hooks return).
export function jsonResponse(status: number, value: unknown): string {
  return JSON.stringify({
    verdict: "allow",
    payload: {
      status,
      headers: { "content-type": "application/json" },
      body: JSON.stringify(value),
    },
  });
}

/// Wrap an HTML body as a `Verdict::Allow` HTTP response.
export function htmlResponse(status: number, body: string): string {
  return JSON.stringify({
    verdict: "allow",
    payload: {
      status,
      headers: { "content-type": "text/html; charset=utf-8" },
      body,
    },
  });
}

export function errMsg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
