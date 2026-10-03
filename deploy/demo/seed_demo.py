#!/usr/bin/env python3
"""Seed the Peckboard App Review demo box. Idempotent; stdlib only.

Run ON the demo host, as the service user (it writes the fake workspace
files the demo folders point at):

  sudo -u peckboard env PECKBOARD_ADMIN_PASSWORD=... \\
      python3 /opt/peckboard-demo/seed_demo.py [options]

Each run:
  - completes first-run setup, pins the agent sandbox to `enforce`, and sets
    the default model to `mock:happy-path`;
  - refuses to continue if any non-mock model is registered;
  - creates the NON-admin reviewer `appreview` (password printed ONCE);
  - seeds fake demo content once (2 folders, 2 projects with cards across
    workflow steps, 6 reviewer-owned sessions, 1 report);
  - enables Remote Access and creates ONE pairing "App Review" (link printed
    ONCE).

Re-runs skip whatever already exists. Options:
  --url URL                  server base URL (default http://127.0.0.1:3344)
  --workspace-root DIR       where demo folders live (default /opt/peckboard-demo/workspaces)
  --rotate-reviewer-password new random reviewer password (old one stops working)
  --new-pairing              revoke every "App Review" pairing and mint a new link
  --self-test                log in as the reviewer and verify the demo end to end
  --disable-remote-access    turn Remote Access off and revoke every pairing, then exit

Env: PECKBOARD_ADMIN_USER (default "admin"), PECKBOARD_ADMIN_PASSWORD (required),
     PECKBOARD_REVIEWER_PASSWORD (only for --self-test on a later run).
"""

import argparse
import json
import os
import secrets
import string
import sys
import time
import urllib.error
import urllib.request

REVIEWER = "appreview"
PAIRING_NAME = "App Review"
DEFAULT_MODEL = "mock:happy-path"
MARKER_FOLDER = "kestrel-web"  # its presence means content is already seeded


# ── HTTP ────────────────────────────────────────────────────────────


class Api:
    def __init__(self, base, token=None):
        self.base = base.rstrip("/")
        self.token = token

    def call(self, method, path, body=None, ok=(200, 201, 202, 204)):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.base + path, data=data, method=method)
        req.add_header("Content-Type", "application/json")
        if self.token:
            req.add_header("Authorization", "Bearer " + self.token)
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                raw, status = r.read(), r.status
        except urllib.error.HTTPError as e:
            raw, status = e.read(), e.code
        if status not in ok:
            raise RuntimeError(f"{method} {path} -> {status}: {raw[:300]!r}")
        return json.loads(raw) if raw.strip() else {}

    @classmethod
    def login(cls, base, username, password):
        api = cls(base)
        api.token = api.call("POST", "/api/auth/login",
                             {"username": username, "password": password})["token"]
        return api


def items(resp, key):
    """List endpoints return a bare array, {key: [...]}, or a page {items: [...]}."""
    if isinstance(resp, list):
        return resp
    return resp.get(key, resp.get("items", []))


def random_password(n=24):
    alphabet = string.ascii_letters + string.digits
    return "".join(secrets.choice(alphabet) for _ in range(n))


def banner(title, lines):
    print("\n" + "=" * 64)
    print(title)
    for line in lines:
        print("  " + line)
    print("=" * 64 + "\n")


# ── Host settings ───────────────────────────────────────────────────


def configure_host(admin):
    models = [m["id"] for m in items(admin.call("GET", "/api/models"), "models")]
    real = [m for m in models if not m.startswith("mock:")]
    if real:
        sys.exit(f"refusing: non-mock models registered ({', '.join(real[:5])}). "
                 "The demo must run with PECKBOARD_PREINSTALL_PLUGINS=mock on a fresh data dir.")
    if not models:
        sys.exit("refusing: no models registered (mock plugin not installed?)")
    print(f"models: {len(models)} mock-only")

    admin.call("POST", "/api/settings/setup/complete")
    assert admin.call("GET", "/api/settings/setup")["completed"] is True
    print("first-run setup: completed")

    sb = admin.call("PUT", "/api/settings/agent-sandbox/config",
                    {"mode": "enforce", "extra_rw": []})
    print(f"agent sandbox: mode={sb['mode']} status={json.dumps(sb.get('status'))}")

    admin.call("PUT", "/api/settings/default-model", {"model": DEFAULT_MODEL})
    print(f"default model: {DEFAULT_MODEL}")


def ensure_reviewer(admin, rotate):
    """Return the reviewer's password when this run set one, else None."""
    users = items(admin.call("GET", "/api/users"), "users")
    existing = next((u for u in users if u["username"] == REVIEWER), None)
    if existing and existing.get("role") == "admin":
        sys.exit(f"refusing: {REVIEWER} exists as an ADMIN; delete it first")
    if existing and not rotate:
        print(f"reviewer: {REVIEWER} exists (password unchanged)")
        return None
    pw = random_password()
    if existing:
        admin.call("PUT", f"/api/users/{existing['id']}/password", {"new_password": pw})
        banner("REVIEWER PASSWORD ROTATED (shown once)", [f"username: {REVIEWER}", f"password: {pw}"])
    else:
        u = admin.call("POST", "/api/users", {"username": REVIEWER, "password": pw, "role": "user"})
        assert u["role"] == "user", u
        banner("REVIEWER ACCOUNT (shown once)", [f"username: {REVIEWER}", f"password: {pw}"])
    return pw


# ── Demo content ────────────────────────────────────────────────────

WORKSPACES = {
    "kestrel-web": {
        "README.md": "# Kestrel Web (demo)\n\nFictional storefront used to demo Peckboard. "
                     "No real customers, code, or data.\n",
        "src/pricing.ts": (
            "// Fictional demo code.\n"
            "export interface LineItem { sku: string; qty: number; unitCents: number }\n\n"
            "export function subtotal(items: LineItem[]): number {\n"
            "  return items.reduce((sum, i) => sum + i.qty * i.unitCents, 0)\n}\n\n"
            "export function applyDiscount(cents: number, pct: number): number {\n"
            "  return Math.round(cents * (1 - pct / 100))\n}\n"),
        "src/checkout.test.ts": (
            "import { subtotal, applyDiscount } from './pricing'\n\n"
            "test('subtotal', () => {\n"
            "  expect(subtotal([{ sku: 'A', qty: 2, unitCents: 450 }])).toBe(900)\n})\n\n"
            "test('discount rounds half up', () => {\n"
            "  expect(applyDiscount(999, 15)).toBe(849)\n})\n"),
    },
    "kestrel-infra": {
        "README.md": "# Kestrel Infra (demo)\n\nFictional ops scripts for the Peckboard demo.\n",
        "scripts/healthcheck.sh": "#!/bin/sh\n# Fictional demo script.\necho \"all services healthy\"\n",
        "dashboards/latency.sql": (
            "-- Fictional demo query.\nSELECT route, percentile_cont(0.95) WITHIN GROUP "
            "(ORDER BY ms) AS p95\nFROM requests GROUP BY route ORDER BY p95 DESC LIMIT 10;\n"),
    },
}

PROJECTS = [
    ("Checkout Revamp", "kestrel-web",
     "Modernise the (fictional) Kestrel storefront checkout: faster, fewer steps, better tests.",
     [("backlog", "Add Apple Pay button to the cart page", "Show the button only on supported devices."),
      ("backlog", "Persist cart across sessions", "Store the cart server-side for signed-in users."),
      ("backlog", "Localise currency formatting", "Use Intl.NumberFormat with the shopper's locale."),
      ("in_progress", "Split checkout into two steps", "Address + payment on separate screens."),
      ("in_progress", "Fix flaky discount rounding test", "applyDiscount rounds inconsistently."),
      ("review", "Inline address validation", "Validate postcode format before submit."),
      ("done", "Remove legacy coupon endpoint", "Endpoint unused since v2.1."),
      ("done", "Add checkout funnel metrics", "Emit step-entered / step-completed events.")]),
    ("Observability", "kestrel-infra",
     "Give the (fictional) Kestrel platform useful dashboards and alerts.",
     [("backlog", "Alert on p95 latency regressions", "Page when p95 > 400 ms for 10 minutes."),
      ("backlog", "Retain traces for 14 days", "Currently 3 days; check storage cost first."),
      ("in_progress", "Dashboard for slow queries", "Top 10 routes by p95 latency."),
      ("review", "Health-check script for all services", "Used by the deploy pipeline."),
      ("done", "Ship structured JSON logs", "All services log JSON to stdout.")]),
]


def ev(kind, **data):
    return (kind, data)


def tool(tid, name, inp, out):
    return [ev("agent-tool-start", name=name, input=inp, toolUseId=tid),
            ev("agent-tool-end", toolUseId=tid, output=out, error=None, images=[])]


def turn(model, prompt, *body, usage=None):
    evs = [ev("user", text=prompt), ev("agent-start", model=model, ownerModel=model)]
    for b in body:
        evs.extend(b if isinstance(b, list) else [b])
    if usage:
        inp, outp = usage
        evs.append(ev("agent-usage", model=model, inputTokens=inp, outputTokens=outp,
                      cacheReadTokens=0, cacheCreationTokens=0, contextTokens=inp,
                      totalTokens=inp + outp, turnSeq=None))
    evs.append(ev("agent-end", ownerModel=model, status="complete"))
    return evs


def text(t):
    return ev("agent-text", text=t)


M = DEFAULT_MODEL
SESSIONS = [
    ("Fix flaky discount rounding test", "kestrel-web", [
        turn(M, "The discount rounding test fails about one run in five. Can you find out why and fix it?",
             text("I'll look at the pricing module and the failing test first."),
             tool("t1", "mcp__peckboard__read_file", {"path": "src/pricing.ts"},
                  "export function applyDiscount(cents: number, pct: number): number {\n"
                  "  return Math.round(cents * (1 - pct / 100))\n}"),
             tool("t2", "mcp__peckboard__run_tests", {"command": "npm test -- checkout"},
                  "FAIL src/checkout.test.ts\n  discount rounds half up\n    Expected: 849\n    Received: 850"),
             text("The bug is floating-point error: `999 * 0.85` is `849.1499999…` on some inputs and "
                  "`849.15` on others, so `Math.round` flips. I'll do the maths in integer cents."),
             tool("t3", "mcp__peckboard__edit_file", {"path": "src/pricing.ts"},
                  '{"ok":true,"path":"src/pricing.ts","edits_applied":1}'),
             tool("t4", "mcp__peckboard__run_tests", {"command": "npm test -- checkout"},
                  "PASS src/checkout.test.ts (2 tests)\n\nRan 50 times: 50 passed."),
             text("Fixed. `applyDiscount` now computes `cents * (100 - pct)` and rounds once at the "
                  "end, so the result no longer depends on floating-point noise. The test passed "
                  "50 runs in a row."),
             usage=(18400, 1250)),
        turn(M, "Nice. Add a test for a 100% discount too.",
             tool("t5", "mcp__peckboard__edit_file", {"path": "src/checkout.test.ts"},
                  '{"ok":true,"path":"src/checkout.test.ts","edits_applied":1}'),
             tool("t6", "mcp__peckboard__run_tests", {"command": "npm test -- checkout"},
                  "PASS src/checkout.test.ts (3 tests)"),
             text("Added `discount of 100% is free` — it asserts `applyDiscount(999, 100) === 0`. "
                  "All 3 tests pass."),
             usage=(21000, 420)),
    ]),
    ("Explain the pricing module", "kestrel-web", [
        turn(M, "Give me a quick tour of src/pricing.ts — I'm new to this codebase.",
             tool("t1", "mcp__peckboard__file_outline", {"path": "src/pricing.ts"},
                  "interface LineItem (2)\nfn subtotal (4-6)\nfn applyDiscount (8-10)"),
             text("**`src/pricing.ts`** is small and pure — no I/O, easy to test.\n\n"
                  "| Symbol | What it does |\n|---|---|\n"
                  "| `LineItem` | One cart row: SKU, quantity, unit price in cents |\n"
                  "| `subtotal(items)` | Sums `qty × unitCents` over the cart |\n"
                  "| `applyDiscount(cents, pct)` | Applies a percentage discount, rounded to whole cents |\n\n"
                  "Prices are integers in **cents** everywhere, which avoids floating-point "
                  "rounding surprises. Tests live in `src/checkout.test.ts`."),
             usage=(9200, 610)),
    ]),
    ("Draft release notes for v2.4", "kestrel-web", [
        turn(M, "Draft customer-facing release notes for v2.4 from the cards we finished.",
             tool("t1", "mcp__peckboard__list_cards", {"step": "done"},
                  "Remove legacy coupon endpoint\nAdd checkout funnel metrics"),
             text("## Kestrel v2.4\n\n"
                  "### Improvements\n"
                  "- **Smoother checkout analytics** — we now measure each checkout step so we can "
                  "keep making it faster.\n"
                  "- **Cleaner coupons** — the old coupon flow has been retired; existing codes keep "
                  "working through the new one.\n\n"
                  "### Fixes\n- Discounts are now rounded consistently to the cent.\n\n"
                  "_Want me to shorten this for the App Store \"What's New\" field?_"),
             usage=(7600, 380)),
    ]),
    ("Investigate slow dashboard queries", "kestrel-infra", [
        turn(M, "The latency dashboard takes ~8 s to load. What's slow?",
             tool("t1", "mcp__peckboard__read_file", {"path": "dashboards/latency.sql"},
                  "SELECT route, percentile_cont(0.95) WITHIN GROUP (ORDER BY ms) AS p95\n"
                  "FROM requests GROUP BY route ORDER BY p95 DESC LIMIT 10;"),
             tool("t2", "mcp__peckboard__run_command",
                  {"command": "psql", "args": ["-c", "EXPLAIN ANALYZE <latency query>"],
                   "reason": "See where the query spends its time."},
                  "Seq Scan on requests  (rows=41,203,118)  (actual time=0.02..7412.9)\n"
                  "Planning Time: 0.3 ms\nExecution Time: 7981.4 ms"),
             text("It scans all 41M rows of `requests` on every load. Two fixes, cheapest first:\n\n"
                  "1. Add a time window — `WHERE ts > now() - interval '24 hours'` — plus an index "
                  "on `(ts)`. Expected: well under 200 ms.\n"
                  "2. Precompute hourly p95 per route in a materialised view refreshed every 5 min.\n\n"
                  "I'd start with (1). Shall I open a card for it?"),
             usage=(12800, 540)),
    ]),
    ("Write the staging certificate runbook", "kestrel-infra", [
        turn(M, "Write a short runbook for rotating the staging TLS certificate.",
             text("# Runbook: Rotate the Staging TLS Certificate\n\n"
                  "**When:** 14 days before expiry (the alert fires automatically).\n\n"
                  "1. Request a new certificate from the internal CA for `staging.kestrel.example`.\n"
                  "2. Upload it to the secrets store under `staging/tls`.\n"
                  "3. Roll the edge proxies one at a time; each picks up the new secret on restart.\n"
                  "4. Verify: `curl -vI https://staging.kestrel.example` shows the new expiry date.\n"
                  "5. Close the alert and note the new expiry in the ops log.\n\n"
                  "**Rollback:** re-upload the previous certificate and roll the proxies again."),
             usage=(5400, 470)),
    ]),
]

REPORT_SESSION = ("Weekly status report", "kestrel-web")
REPORT_BODY = """# Kestrel — Weekly Status (Demo)

_All names and numbers in this report are fictional demo data._

## Done This Week
- Removed the legacy coupon endpoint.
- Added checkout funnel metrics.
- Shipped structured JSON logs on every service.

## In Progress
- Two-step checkout (address, then payment).
- Slow-query dashboard: the fix is a 24-hour window plus an index.

## Risks
- Trace retention is 3 days; raising it to 14 needs a storage cost check.
"""


def create_workspaces(root):
    for folder, files in WORKSPACES.items():
        for rel, content in files.items():
            path = os.path.join(root, folder, rel)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            if not os.path.exists(path):
                with open(path, "w") as f:
                    f.write(content)


def wait_turn_end(api, sid, want_ends, timeout=60):
    end = time.time() + timeout
    while time.time() < end:
        evs = items(api.call("GET", f"/api/sessions/{sid}/events?limit=500"), "events")
        if sum(1 for e in evs if e["kind"] == "agent-end") >= want_ends:
            return evs
        time.sleep(0.5)
    raise RuntimeError(f"session {sid}: no agent-end after {timeout}s")


def seed_content(base, admin, reviewer_pw, root):
    folders = {f["name"]: f for f in items(admin.call("GET", "/api/folders"), "folders")}
    if MARKER_FOLDER in folders:
        print("content: already seeded (skipping)")
        return
    if not reviewer_pw:
        sys.exit("content missing but the reviewer password is unknown; "
                 "re-run with --rotate-reviewer-password")
    create_workspaces(root)
    for name in WORKSPACES:
        if name not in folders:
            folders[name] = admin.call("POST", "/api/folders",
                                       {"name": name, "path": os.path.join(root, name)})
    print(f"folders: {', '.join(WORKSPACES)}")

    for pname, fname, context, cards in PROJECTS:
        p = admin.call("POST", "/api/projects", {
            "name": pname, "folder_id": folders[fname]["id"], "model": DEFAULT_MODEL,
            "context": context, "worker_count": 1, "workflow": "task"})
        # Paused: no worker ever dispatches on demo cards.
        admin.call("POST", f"/api/projects/{p['id']}/pause", {})
        for i, (step, title, desc) in enumerate(cards):
            admin.call("POST", f"/api/projects/{p['id']}/cards",
                       {"title": title, "description": desc, "step": step, "priority": i % 3})
        print(f"project: {pname} ({len(cards)} cards)")

    # Sessions are per-user: the reviewer creates them so they own them; the
    # admin appends the scripted transcript (only admins may post agent events).
    rev = Api.login(base, REVIEWER, reviewer_pw)
    for name, fname, turns in SESSIONS:
        s = rev.call("POST", "/api/sessions",
                     {"name": name, "folder_id": folders[fname]["id"], "model": DEFAULT_MODEL})
        for t in turns:
            for kind, data in t:
                if kind.startswith("agent-tool"):
                    data = dict(data, toolUseId=f"{s['id'][:8]}-{data['toolUseId']}")
                admin.call("POST", f"/api/sessions/{s['id']}/events", {"kind": kind, "data": data})
        print(f"session: {name}")

    # The report comes from a REAL mock turn calling the write_report tool,
    # so it is linked to its session like an agent-written one.
    name, fname = REPORT_SESSION
    s = rev.call("POST", "/api/sessions",
                 {"name": name, "folder_id": folders[fname]["id"], "model": DEFAULT_MODEL})
    block = json.dumps({"tool": "write_report",
                        "args": {"title": "Kestrel weekly status", "body": REPORT_BODY}})
    rev.call("POST", f"/api/sessions/{s['id']}/message", {
        "text": f"Publish this week's status report.\n\n```mcp\n{block}\n```",
        "model": "mock:mcp"})
    evs = wait_turn_end(rev, s["id"], 1)
    errs = [e["data"].get("error") for e in evs if e["kind"] == "agent-tool-end" and e["data"].get("error")]
    if errs:
        raise RuntimeError(f"write_report failed: {errs}")
    print(f"session: {name} (+ report)")


# ── Remote access ───────────────────────────────────────────────────


def ensure_pairing(admin, new_pairing):
    ra = admin.call("GET", "/api/remote-access")
    if not ra["enabled"]:
        ra.update(admin.call("PUT", "/api/remote-access", {"enabled": True}))
    print(f"remote access: enabled via {ra['relay_host']}")
    mine = [d for d in ra["devices"] if d["name"] == PAIRING_NAME]
    if mine and not new_pairing:
        print(f"pairing: '{PAIRING_NAME}' exists (link not recoverable; use --new-pairing)")
        return
    for d in mine:
        admin.call("DELETE", f"/api/remote-access/devices/{d['id']}")
        print(f"pairing: revoked old '{PAIRING_NAME}' ({d['id'][:8]})")
    r = admin.call("POST", "/api/remote-access/devices", {"name": PAIRING_NAME})
    banner("APP REVIEW PAIRING LINK (shown once)", [r["pairing_link"]])


def disable_remote_access(admin):
    ra = admin.call("GET", "/api/remote-access")
    for d in ra["devices"]:
        admin.call("DELETE", f"/api/remote-access/devices/{d['id']}")
    admin.call("PUT", "/api/remote-access", {"enabled": False})
    print(f"remote access: disabled, {len(ra['devices'])} pairing(s) revoked")


# ── Self-test ───────────────────────────────────────────────────────


def self_test(base, admin, reviewer_pw):
    if not reviewer_pw:
        sys.exit("--self-test needs the reviewer password (this run's, or PECKBOARD_REVIEWER_PASSWORD)")
    rev = Api.login(base, REVIEWER, reviewer_pw)
    me = rev.call("GET", "/api/auth/me")
    role = me.get("role") or me.get("user", {}).get("role")
    assert role == "user", f"reviewer role is {role!r}"
    rev.call("GET", "/api/remote-access", ok=(403,))  # admin-only surface is closed
    models = [m["id"] for m in items(rev.call("GET", "/api/models"), "models")]
    assert models and all(m.startswith("mock:") for m in models), models
    sessions = items(rev.call("GET", "/api/sessions?limit=100"), "sessions")
    projects = items(rev.call("GET", "/api/projects"), "projects")
    reports = items(rev.call("GET", "/api/reports"), "reports")
    assert len(sessions) >= 6 and len(projects) >= 2 and reports, (len(sessions), len(projects), len(reports))
    # A live mock reply as the reviewer, in a throwaway session.
    folder = next(f for f in items(rev.call("GET", "/api/folders"), "folders") if f["name"] == MARKER_FOLDER)
    s = rev.call("POST", "/api/sessions", {"name": "self-test", "folder_id": folder["id"]})
    try:
        rev.call("POST", f"/api/sessions/{s['id']}/message", {"text": "Hello from the self-test"})
        evs = wait_turn_end(rev, s["id"], 1)
        texts = [e["data"].get("text") for e in evs if e["kind"] == "agent-text"]
        start = next(e for e in evs if e["kind"] == "agent-start")
        assert texts, "no agent-text in reply"
        reply_model = start["data"].get("model")
    finally:
        rev.call("DELETE", f"/api/sessions/{s['id']}")
    ra = admin.call("GET", "/api/remote-access")
    pairings = [d for d in ra["devices"] if d["name"] == PAIRING_NAME]
    print(f"self-test OK: role={role} models={len(models)} (all mock) sessions={len(sessions)} "
          f"projects={len(projects)} reports={len(reports)} reply[{reply_model}]={texts!r} "
          f"remote_access={ra['enabled']} pairings={len(pairings)}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--url", default="http://127.0.0.1:3344")
    ap.add_argument("--workspace-root", default="/opt/peckboard-demo/workspaces")
    ap.add_argument("--rotate-reviewer-password", action="store_true")
    ap.add_argument("--new-pairing", action="store_true")
    ap.add_argument("--self-test", action="store_true")
    ap.add_argument("--disable-remote-access", action="store_true")
    a = ap.parse_args()

    admin_pw = os.environ.get("PECKBOARD_ADMIN_PASSWORD")
    if not admin_pw:
        sys.exit("set PECKBOARD_ADMIN_PASSWORD (see README: --reset-password)")
    admin = Api.login(a.url, os.environ.get("PECKBOARD_ADMIN_USER", "admin"), admin_pw)
    if a.disable_remote_access:
        disable_remote_access(admin)
        return

    configure_host(admin)
    reviewer_pw = ensure_reviewer(admin, a.rotate_reviewer_password)
    seed_content(a.url, admin, reviewer_pw, os.path.abspath(a.workspace_root))
    ensure_pairing(admin, a.new_pairing)
    if a.self_test:
        self_test(a.url, admin, reviewer_pw or os.environ.get("PECKBOARD_REVIEWER_PASSWORD"))


if __name__ == "__main__":
    main()
