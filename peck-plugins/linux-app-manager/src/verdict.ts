// Response envelope helpers for the mcp.tool.invoke hook. Pure — no Extism
// runtime dependency, so these load under vitest without a Host/Memory shim.

export function skip(): string {
  return JSON.stringify({ verdict: "skip" });
}

export function allow(value: unknown): string {
  return JSON.stringify({ verdict: "allow", payload: value });
}

export function cancel(reason: string): string {
  return JSON.stringify({ verdict: "cancel", reason });
}

export function errMsg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
