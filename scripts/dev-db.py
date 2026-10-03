#!/usr/bin/env python3
"""Dev-DB fixture builder + migration checker. Driven by scripts/dev-db.sh.

  seed    <binary> <dir>   boot <binary> on empty <dir>, seed realistic data via HTTP
  migrate <binary> <dir>   snapshot counts, boot <binary> on <dir>, stop, diff

Every server boots with PECKBOARD_NO_RESUME=1 PECKBOARD_TTS_DOWNLOAD=0 on a random
high port (+1 for https), and only the PID started here is ever signalled.
Never point <dir> at a real install (~/.peckboard) or a copy of one.
"""

import json
import os
import random
import signal
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.request

USER = "dev-admin"
PASS = "dev-password-1234"


# ── Server lifecycle ────────────────────────────────────────────────


class Server:
    def __init__(self, binary, data_dir, extra_env=None):
        self.port = random.randint(20000, 29990)
        self.base = f"http://127.0.0.1:{self.port}"
        env = dict(os.environ)
        env.update(
            PECKBOARD_NO_RESUME="1",
            PECKBOARD_TTS_DOWNLOAD="0",
            PECKBOARD_CLAUDE_MODEL_DISCOVERY="0",
        )
        env.update(extra_env or {})
        self.log_path = os.path.join(data_dir, f"server-{int(time.time())}.log")
        self.log = open(self.log_path, "w")
        self.proc = subprocess.Popen(
            [binary, "--data-dir", data_dir, "--port", str(self.port),
             "--https-port", str(self.port + 1)],
            env=env, stdout=self.log, stderr=subprocess.STDOUT,
        )
        print(f"  booted pid={self.proc.pid} port={self.port} log={self.log_path}")
        self.wait_health()

    def wait_health(self, timeout=90):
        end = time.time() + timeout
        while time.time() < end:
            if self.proc.poll() is not None:
                sys.exit(f"server exited early ({self.proc.returncode}); see {self.log_path}")
            try:
                with urllib.request.urlopen(self.base + "/api/health", timeout=2) as r:
                    if r.status == 200:
                        return
            except Exception:
                pass
            time.sleep(0.5)
        self.stop()
        sys.exit(f"health timeout; see {self.log_path}")

    def stop(self):
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()


class Api:
    def __init__(self, base):
        self.base = base
        self.token = None

    def call(self, method, path, body=None, ok=(200, 201, 202, 204)):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.base + path, data=data, method=method)
        req.add_header("Content-Type", "application/json")
        if self.token:
            req.add_header("Authorization", "Bearer " + self.token)
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                raw = r.read()
                status = r.status
        except urllib.error.HTTPError as e:
            raw, status = e.read(), e.code
        if status not in ok:
            raise RuntimeError(f"{method} {path} -> {status}: {raw[:300]!r}")
        return json.loads(raw) if raw.strip() else {}

    def login(self):
        self.token = self.call("POST", "/api/auth/login",
                               {"username": USER, "password": PASS})["token"]


# ── Seeding ─────────────────────────────────────────────────────────


def wait_turns(db_path, session_id, want, timeout=60):
    """Block until `session_id` has `want` agent-end events (read-only DB peek)."""
    end = time.time() + timeout
    while time.time() < end:
        db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True, timeout=5)
        try:
            n = db.execute("select count(*) from events where session_id=? and kind='agent-end'",
                           (session_id,)).fetchone()[0]
        finally:
            db.close()
        if n >= want:
            return
        time.sleep(0.2)
    raise RuntimeError(f"session {session_id}: timed out waiting for {want} agent-end")


PROMPTS = [
    "Refactor the parser so the tokenizer and the AST builder live in separate modules, "
    "then add a regression test for the trailing-comma bug reported last week.",
    "Summarise the open TODOs in this folder and propose an order to tackle them in.",
    "Why does the release build take twice as long as the debug build here? Check the profile.",
    "Write a short changelog entry for the voice assistant improvements.",
    "Check whether the migration adds an index for the new lookup column.",
]


def seed(binary, data_dir):
    if os.path.exists(os.path.join(data_dir, "peckboard.db")):
        sys.exit(f"{data_dir} already holds a peckboard.db; seed wants an empty dir")
    os.makedirs(data_dir, exist_ok=True)
    # Folder targets live NEXT TO the data dir so copying the data dir for a
    # migrate run never duplicates them and the fixture stays self-describing.
    proj_root = os.path.abspath(data_dir.rstrip("/") + "-projects")
    srv = Server(binary, data_dir, {
        "PECKBOARD_BOOTSTRAP_USERNAME": USER,
        "PECKBOARD_BOOTSTRAP_PASSWORD": PASS,
        "PECKBOARD_PREINSTALL_PLUGINS": "all",
    })
    db_path = os.path.join(data_dir, "peckboard.db")
    try:
        api = Api(srv.base)
        api.login()

        folders = []
        for name in ("webshop", "infra-scripts", "notes"):
            path = os.path.join(proj_root, name)
            os.makedirs(path, exist_ok=True)
            with open(os.path.join(path, "README.md"), "w") as f:
                f.write(f"# {name}\n\nDev-DB fixture folder.\n")
            folders.append(api.call("POST", "/api/folders", {"name": name, "path": path}))
        print(f"  folders: {len(folders)}")

        api.call("POST", "/api/env-vars", {"name": "DEV_DB_GLOBAL", "value": "hello"})
        api.call("POST", "/api/env-vars", {"name": "DEV_DB_SCOPED", "value": "scoped-value",
                                           "folder_id": folders[0]["id"]})

        projects = []
        for i, name in enumerate(("Webshop Revamp", "Infra Cleanup")):
            p = api.call("POST", "/api/projects", {
                "name": name, "folder_id": folders[i]["id"], "model": "mock:happy-path",
                "context": f"{name}: dev-db fixture project.", "worker_count": 2,
                "workflow": "task",
            })
            # Paused so no orchestrator ever dispatches a worker on these cards.
            api.call("POST", f"/api/projects/{p['id']}/pause", {})
            projects.append(p)
        steps = ["backlog"] * 4 + ["in_progress"] * 2 + ["review"] + ["done"] * 2
        ncards = 0
        prev = None
        for p in projects:
            for j, step in enumerate(steps):
                body = {"title": f"{p['name']} task {j + 1}",
                        "description": f"Do part {j + 1} of {p['name']}.\n\n- acceptance: tests pass",
                        "step": step, "priority": j % 3}
                if step in ("in_progress", "review"):
                    body.update(blocked=True, block_reason="fixture: keep workers off")
                if prev and step == "backlog" and j == 3:
                    body["depends_on"] = [prev]
                c = api.call("POST", f"/api/projects/{p['id']}/cards", body)
                prev = c["id"]
                ncards += 1
        print(f"  projects: {len(projects)}, cards: {ncards}")

        api.call("POST", "/api/repeating-tasks", {
            "name": "Nightly dependency audit", "folder_id": folders[1]["id"],
            "prompt": "Audit dependencies for known CVEs.", "schedule_kind": "daily",
            "schedule_value": {"hour": 3, "minute": 0}, "model": "mock:happy-path",
            "enabled": False,
        })

        sessions = []
        for i in range(20):
            s = api.call("POST", "/api/sessions", {"name": f"Dev session {i + 1}",
                                                   "folder_id": folders[i % 3]["id"]})
            sessions.append(s["id"])
        turns_per = 12  # ~2.5k events
        for t in range(turns_per):
            for i, sid in enumerate(sessions):
                model = ("mock:happy-path", "mock:echo-stream", "mock:echo")[(i + t) % 3]
                api.call("POST", f"/api/sessions/{sid}/message",
                         {"text": PROMPTS[(i + t) % len(PROMPTS)], "model": model})
            for sid in sessions:
                wait_turns(db_path, sid, t + 1)
            time.sleep(0.5)  # let completion listeners settle before the next send
        print(f"  sessions: {len(sessions)} x {turns_per} turns")

        # Queued messages: park 3 sessions in a long turn, queue a follow-up on
        # each, then stop the server mid-turn so the rows persist.
        for sid in sessions[:3]:
            api.call("POST", f"/api/sessions/{sid}/message",
                     {"text": "sleep:30 long running step", "model": "mock:slow"})
        time.sleep(1.5)
        for sid in sessions[:3]:
            api.call("POST", f"/api/sessions/{sid}/message",
                     {"text": "follow-up once you're done", "model": "mock:echo"})
        time.sleep(1)
    finally:
        srv.stop()
    print("  seed done; counts:")
    print_counts(snapshot(db_path))


# ── Counting + migrate ──────────────────────────────────────────────


def snapshot(db_path):
    db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    try:
        tables = [r[0] for r in db.execute(
            "select name from sqlite_master where type='table' and name not like 'sqlite_%' order by name")]
        counts = {t: db.execute(f'select count(*) from "{t}"').fetchone()[0] for t in tables}
        starts = db.execute("select count(*) from events where kind='agent-start'").fetchone()[0]
        version = db.execute("select max(version) from __diesel_schema_migrations").fetchone()[0]
        integrity = db.execute("pragma integrity_check").fetchone()[0]
        max_rowid = db.execute("select coalesce(max(rowid), 0) from events").fetchone()[0]
        # Sessions whose latest agent-start has no agent-end after it.
        open_turns = db.execute(
            "select count(*) from (select session_id,"
            " max(case when kind='agent-start' then rowid end) s,"
            " max(case when kind='agent-end' then rowid end) e"
            " from events group by session_id) where s is not null and (e is null or e < s)"
        ).fetchone()[0]
    finally:
        db.close()
    return {"counts": counts, "agent_starts": starts, "max_version": version,
            "integrity": integrity, "max_event_rowid": max_rowid, "open_turns": open_turns}


def new_events(db_path, after_rowid):
    """(kind, data.reason) of every event appended after `after_rowid`."""
    db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    try:
        return db.execute("select kind, json_extract(data, '$.reason') from events "
                          "where rowid > ? order by rowid", (after_rowid,)).fetchall()
    finally:
        db.close()


def print_counts(s):
    nz = {t: n for t, n in s["counts"].items() if n}
    print("   ", " ".join(f"{t}={n}" for t, n in nz.items()))
    print(f"    agent_starts={s['agent_starts']} max_version={s['max_version']} "
          f"open_turns={s['open_turns']} integrity={s['integrity']} tables={len(s['counts'])}")


def migrate(binary, data_dir, expect_tables, expect_version, settle):
    db_path = os.path.join(data_dir, "peckboard.db")
    if not os.path.exists(db_path):
        sys.exit(f"no peckboard.db in {data_dir}")
    before = snapshot(db_path)
    print("BEFORE:")
    print_counts(before)
    srv = Server(binary, data_dir)
    time.sleep(settle)  # give any unwanted auto-start a chance to show up
    srv.stop()
    after = snapshot(db_path)
    print("AFTER:")
    print_counts(after)

    fails = []
    for t, n in before["counts"].items():
        m = after["counts"].get(t)
        if m is None:
            fails.append(f"table {t} disappeared ({n} rows)")
        elif t == "__diesel_schema_migrations" and m >= n:
            continue  # one row per newly applied migration
        elif t == "events" and m >= n:
            # Boot closes turns a killed server left open with one
            # `agent-end {reason: server-shutdown}` each; anything else is new.
            added = new_events(db_path, before["max_event_rowid"])
            healed = [k for k, r in added if k == "agent-end" and r == "server-shutdown"]
            other = [k for k, r in added if not (k == "agent-end" and r == "server-shutdown")]
            print(f"NEW EVENTS: {len(healed)} agent-end(server-shutdown) closing "
                  f"{before['open_turns']} open turn(s); other: {other or 'none'}")
            if other or len(healed) > before["open_turns"]:
                fails.append(f"events: {n} -> {m} (unexpected: {other})")
        elif m != n:
            fails.append(f"{t}: {n} -> {m}")
            fails.append(f"{t}: {n} -> {m}")
    new = sorted(set(after["counts"]) - set(before["counts"]))
    print(f"NEW TABLES: {', '.join(new) or '(none)'}")
    for t in expect_tables:
        if t not in after["counts"]:
            fails.append(f"expected table {t} missing")
    if after["agent_starts"] != before["agent_starts"]:
        fails.append(f"agent starts {before['agent_starts']} -> {after['agent_starts']}")
    if expect_version and str(after["max_version"]) != str(expect_version):
        fails.append(f"max migration {after['max_version']} != {expect_version}")
    if after["integrity"] != "ok":
        fails.append(f"integrity_check: {after['integrity']}")
    with open(srv.log_path) as f:
        if "PECKBOARD_NO_RESUME" not in f.read():
            fails.append("server log lacks the PECKBOARD_NO_RESUME line")
    if fails:
        print("FAIL")
        for f in fails:
            print("  -", f)
        sys.exit(1)
    print("PASS")


def main():
    a = sys.argv[1:]
    if len(a) < 3 or a[0] not in ("seed", "migrate"):
        sys.exit(__doc__)
    cmd, binary, data_dir = a[0], os.path.abspath(a[1]), os.path.abspath(a[2])
    if os.path.realpath(data_dir).startswith(os.path.realpath(os.path.expanduser("~/.peckboard"))):
        sys.exit("refusing to touch ~/.peckboard")
    if cmd == "seed":
        seed(binary, data_dir)
    else:
        opts = dict(zip(a[3::2], a[4::2]))
        migrate(binary, data_dir,
                [t for t in opts.get("--expect-tables", "").split(",") if t],
                opts.get("--expect-version"),
                float(opts.get("--settle", "10")))


if __name__ == "__main__":
    main()
