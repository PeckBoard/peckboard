// Query-string parsing shared by the data routes (http.ts) and the page's
// deep-link prefill (deeplink.ts). Kept in its own module so deeplink.ts
// doesn't have to import http.ts, which imports the page.

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
