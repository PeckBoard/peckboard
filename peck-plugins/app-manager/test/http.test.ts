// The dashboard's HTTP surface: the public page route and the authenticated
// data routes the page calls through the parent-proxied fetch bridge. Covers
// route matching, the target CRUD the page owns (no MCP tool does it), the
// app-grid payload, and the promise that no error ever leaves as raw JSON.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { installHost } from "./hostShim";
import { queryParam, serveAuthed, serveHttp } from "../src/http";
import { dispatch } from "../src/lib";

const API = "/api/plugin-ui/app-manager";
const PAGE_PATH = "/plugin-api/v1/app-manager";

/** Unwrap the `{verdict, payload:{status, body}}` envelope into status + JSON. */
function res(json: string): { status: number; body: any } {
  const v = JSON.parse(json);
  expect(v.verdict).toBe("allow");
  return { status: v.payload.status, body: JSON.parse(v.payload.body) };
}

function get(path: string, query = ""): { status: number; body: any } {
  return res(serveAuthed({ method: "GET", path, query, body: "" }));
}
function post(path: string, body: unknown): { status: number; body: any } {
  return res(
    serveAuthed({
      method: "POST",
      path,
      query: "",
      body: JSON.stringify(body),
    }),
  );
}

const OK_EXEC = {
  exit_code: 0,
  stdout: "",
  stderr: "",
  stdout_truncated: false,
  stderr_truncated: false,
  timed_out: false,
};

/** Local exec mock: answers the os-release probe, reports every app missing. */
function localExec(osRelease = "ID=ubuntu\nID_LIKE=debian\n") {
  return (input: { args: string[] }) => {
    const script = input.args[1] || "";
    if (script.includes("/etc/os-release")) {
      return { ...OK_EXEC, stdout: osRelease };
    }
    return { ...OK_EXEC, exit_code: 1 };
  };
}

describe("the page route", () => {
  beforeEach(() => installHost({}));

  it("serves the dashboard HTML", () => {
    const v = JSON.parse(serveHttp({ method: "GET", path: PAGE_PATH }));
    expect(v.payload.status).toBe(200);
    expect(v.payload.headers["content-type"]).toContain("text/html");
    expect(v.payload.body).toContain("App Manager");
  });

  it("404s any other path", () => {
    const v = JSON.parse(
      serveHttp({ method: "GET", path: "/plugin-api/v1/nope" }),
    );
    expect(v.payload.status).toBe(404);
  });

  it("is reachable through the hook dispatcher", () => {
    const v = JSON.parse(
      dispatch("http.request.before", { method: "GET", path: PAGE_PATH }),
    );
    expect(v.payload.status).toBe(200);
  });
});

describe("authed data routes", () => {
  beforeEach(() => installHost({}));

  it("404s an unknown route with a readable message", () => {
    const r = get(`${API}/nope`);
    expect(r.status).toBe(404);
    expect(r.body.error).toBe("Not found.");
  });

  it("404s a known path used with the wrong method", () => {
    expect(post(`${API}/apps`, {}).status).toBe(404);
  });

  it("lists the local target", () => {
    const r = get(`${API}/targets`);
    expect(r.status).toBe(200);
    expect(r.body.targets).toEqual([
      {
        id: "local",
        kind: "local",
        label: "Local (this host)",
        detail: "this Peckboard host",
      },
    ]);
  });

  it("returns vault key metadata only, never key material", () => {
    installHost(
      {},
      {
        sshKeyList: () => ({
          keys: [
            {
              id: "k1",
              name: "deploy",
              key_type: "ed25519",
              fingerprint: "SHA256:x",
              has_passphrase: false,
              created_at: "",
            },
          ],
        }),
      },
    );
    const r = get(`${API}/ssh-keys`);
    expect(r.body.keys[0].name).toBe("deploy");
    expect(JSON.stringify(r.body)).not.toContain("PRIVATE KEY");
  });

  it("is reachable through the hook dispatcher", () => {
    const v = JSON.parse(
      dispatch("http.request.authed", {
        method: "GET",
        path: `${API}/targets`,
        query: "",
        body: "",
      }),
    );
    expect(v.payload.status).toBe(200);
  });
});

describe("remote target CRUD (owned by the page, not by any MCP tool)", () => {
  beforeEach(() => installHost({}));

  it("creates, lists, edits, and removes a remote target", () => {
    const created = post(`${API}/targets`, {
      label: "build box",
      hostname: "10.0.0.5",
      port: "2222",
      username: "ubuntu",
      key_id: "k1",
    });
    expect(created.status).toBe(200);
    const id = created.body.target.id;
    expect(created.body.target.detail).toBe("ubuntu@10.0.0.5:2222");

    expect(get(`${API}/targets`).body.targets.map((t: any) => t.id)).toEqual([
      "local",
      id,
    ]);

    const edited = post(`${API}/targets`, { id, username: "root" });
    expect(edited.body.target.detail).toBe("root@10.0.0.5:2222");

    expect(post(`${API}/target-remove`, { id }).body.removed).toBe(id);
    expect(get(`${API}/targets`).body.targets).toHaveLength(1);
  });

  it("rejects a target with no vault key, in prose", () => {
    const r = post(`${API}/targets`, {
      hostname: "10.0.0.5",
      username: "ubuntu",
    });
    expect(r.status).toBe(400);
    expect(r.body.error).toContain("key_id is required");
    expect(r.body.error).not.toContain("{");
  });

  it("refuses to remove the local target", () => {
    const r = post(`${API}/target-remove`, { id: "local" });
    expect(r.status).toBe(400);
    expect(r.body.error).toContain("cannot be removed");
  });
});

describe("the app grid payload", () => {
  it("reports the distro and one row per catalog app", () => {
    installHost({}, { execAny: localExec() });
    const r = get(`${API}/apps`, "target=local");
    expect(r.status).toBe(200);
    expect(r.body.distro.supported).toBe(true);
    expect(r.body.distro.package_manager).toBe("apt");
    expect(r.body.apps.length).toBeGreaterThan(0);
    expect(r.body.apps[0]).toMatchObject({
      installed: false,
      action: "install",
      action_label: "Install",
    });
  });

  it("renders a non-Linux target as a refusal and lists no apps", () => {
    installHost({}, { execAny: () => ({ ...OK_EXEC, exit_code: 1 }) });
    const r = get(`${API}/apps`, "target=local");
    expect(r.body.distro.supported).toBe(false);
    expect(r.body.distro.refusal).toContain("Linux");
    expect(r.body.apps).toEqual([]);
  });

  it("returns a full grid row on the status route, so the page can swap a row wholesale", () => {
    installHost({}, { execAny: localExec() });
    const r = get(`${API}/status`, "target=local&app=git");
    expect(r.status).toBe(200);
    expect(r.body.row).toMatchObject({
      id: "git",
      state_label: "Not installed",
      action: "install",
      action_label: "Install",
      actionable: true,
    });
    expect(r.body.job).toBeNull();
  });

  it("rejects an unknown target without running anything", () => {
    const execAny = vi.fn();
    installHost({}, { execAny });
    const r = get(`${API}/apps`, "target=ghost");
    expect(r.status).toBe(400);
    expect(r.body.error).toContain("Unknown target");
    expect(execAny).not.toHaveBeenCalled();
  });

  it("rejects an unknown app on the status route", () => {
    installHost({}, { execAny: localExec() });
    const r = get(`${API}/status`, "target=local&app=not-real");
    expect(r.status).toBe(400);
    expect(r.body.error).toContain("Unknown app");
  });
});

describe("queryParam", () => {
  it("decodes a value and ignores other keys", () => {
    expect(queryParam("target=local&app=git", "app")).toBe("git");
    expect(queryParam("target=my%20box", "target")).toBe("my box");
    expect(queryParam("target=a+b", "target")).toBe("a b");
    expect(queryParam("target=local", "missing")).toBeUndefined();
  });
});
