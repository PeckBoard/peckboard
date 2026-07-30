---
title: OAuth Redirect Broker
parent: Plugins
nav_order: 3
---

# OAuth Redirect Broker

Some providers will not register your PeckBoard instance's own callback URL.
Slack is the clearest case: an app's Redirect URLs must be HTTPS, must match
or prefix the `redirect_uri` sent on the authorize request, and may contain
**no wildcards and no arbitrary ports**. One registered app therefore cannot
serve installs that each live at a different origin — which is why Cursor and
Claude Code each registered their own Slack app with their own fixed redirect
(`cursor://…` and `http://localhost:3118` respectively).

A redirect broker is the way out: one fixed public callback that every
install shares. You register **that** URL with the provider once, and each
PeckBoard pulls its authorization code back from it.

## What it changes

|                                   | Without a broker               | With a broker                    |
| --------------------------------- | ------------------------------ | -------------------------------- |
| `redirect_uri` sent               | `{your origin}/oauth/callback` | `{broker}/callback`              |
| Where the code lands              | this instance                  | the broker, briefly              |
| How the sign-in finishes          | the browser redirect           | PeckBoard polls `{broker}/claim` |
| What the provider must allow-list | every install's origin         | one URL                          |

The broker never sees a token. It holds a single-use authorization code for
about two minutes; the exchange happens on your host, with the PKCE verifier
that never left the process.

## Wiring it up

Settings → MCP Servers → the server's Sign in panel → **Public callback
broker**. Registry entries can ship it too, as `oauth.redirect_broker`:

```json
"oauth": {
  "authorize_url": "https://slack.com/oauth/v2_user/authorize",
  "token_url": "https://slack.com/api/oauth.v2.user.access",
  "redirect_broker": "https://oauth.example.com/slack"
}
```

PeckBoard refuses a broker that is not `https://` (bare `http://localhost` is
allowed for development). Register `{broker}/callback` with the provider —
the panel shows the exact URL once the field is set.

## The contract

Two routes, no state beyond the in-flight code.

`GET {broker}/callback?code=…&state=…` — the provider's redirect. Store the
code (or the provider's `error`) under `state` with a short TTL, then render
a "you can close this tab" page.

`GET {broker}/claim?state=…` — PeckBoard's poll.

- holding a code → `200 {"code": "…"}`, then forget it (**single use**)
- provider denied → `200 {"error": "access_denied"}`
- nothing yet → `404` or `204` (PeckBoard reads this as "keep waiting")

## Reference implementation

A Cloudflare Worker with one KV namespace bound as `CODES`. Any equivalent
runtime works — the whole thing is two handlers and a TTL.

```js
const TTL_SECONDS = 120;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/callback")) return callback(url, env);
    if (url.pathname.endsWith("/claim")) return claim(url, env);
    return new Response("not found", { status: 404 });
  },
};

// The provider's browser redirect lands here.
async function callback(url, env) {
  const state = url.searchParams.get("state") ?? "";
  // States are 32 random bytes, base64url. Anything else is not ours.
  if (state.length < 40 || state.length > 128) {
    return page(
      "Sign-in failed",
      "This redirect is missing a usable state parameter.",
    );
  }
  const error = url.searchParams.get("error");
  const code = url.searchParams.get("code");
  if (!error && !code) {
    return page(
      "Sign-in failed",
      "The redirect carried no authorization code.",
    );
  }
  await env.CODES.put(state, JSON.stringify(error ? { error } : { code }), {
    expirationTtl: TTL_SECONDS,
  });
  return page("Signed in", "You can close this tab and return to PeckBoard.");
}

// PeckBoard polls here while the consent tab is open.
async function claim(url, env) {
  const state = url.searchParams.get("state") ?? "";
  if (!state) return new Response(null, { status: 404 });
  const held = await env.CODES.get(state);
  if (!held) return new Response(null, { status: 404 });
  await env.CODES.delete(state); // single use
  return new Response(held, {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

function page(title, detail) {
  return new Response(
    `<!doctype html><meta charset="utf-8">` +
      `<title>${title}</title>` +
      `<body style="font-family:system-ui,sans-serif;text-align:center;margin-top:15vh">` +
      `<h2>${title}</h2><p>${detail}</p></body>`,
    { status: 200, headers: { "content-type": "text/html; charset=utf-8" } },
  );
}
```

## What the broker is trusted with

Only a live authorization code, for at most the TTL, handed out once.

That code is bound to a PKCE `code_verifier` that never leaves your
PeckBoard, and — for a confidential client — to a client secret the broker
also never sees. So a leaked code alone does not mint a token. The `state`
that keys it is 32 bytes of CSPRNG output, so codes cannot be claimed by
guessing.

It is still a live credential in transit: run the broker on a host you
control, over HTTPS, and keep the TTL short. Deliberately, the broker does
**not** redirect back to the instance — a broker that forwarded codes to an
origin named in the request would be an open redirect pointed at your
authorization codes.
