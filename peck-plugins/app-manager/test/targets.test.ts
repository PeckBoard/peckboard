import { beforeEach, describe, expect, it } from "vitest";
import { installHost } from "./hostShim";
import {
  LOCAL_TARGET,
  buildRecord,
  getTarget,
  listTargets,
  putTarget,
  resolveTarget,
  toConn,
} from "../src/targets";

beforeEach(() => {
  installHost({});
});

describe("buildRecord", () => {
  it("requires hostname, username, and key_id", () => {
    expect(() => buildRecord({}, null, () => "t1")).toThrow(/hostname/);
    expect(() => buildRecord({ hostname: "h" }, null, () => "t1")).toThrow(
      /username/,
    );
    expect(() =>
      buildRecord({ hostname: "h", username: "u" }, null, () => "t1"),
    ).toThrow(/key_id/);
  });

  it("rejects an out-of-range or non-integer port", () => {
    const base = { hostname: "h", username: "u", key_id: "k1" };
    expect(() => buildRecord({ ...base, port: 0 }, null, () => "t1")).toThrow(
      /port/,
    );
    expect(() =>
      buildRecord({ ...base, port: 70000 }, null, () => "t1"),
    ).toThrow(/port/);
    expect(() =>
      buildRecord({ ...base, port: 22.5 }, null, () => "t1"),
    ).toThrow(/port/);
  });

  it("defaults port to 22 and label to hostname", () => {
    const rec = buildRecord(
      { hostname: "box.example.com", username: "root", key_id: "k1" },
      null,
      () => "t1",
    );
    expect(rec).toMatchObject({
      id: "t1",
      kind: "remote",
      label: "box.example.com",
      hostname: "box.example.com",
      port: 22,
      username: "root",
      key_id: "k1",
    });
  });

  it("merges onto an existing record when updating", () => {
    const existing = buildRecord(
      { hostname: "box", username: "root", key_id: "k1", label: "Box" },
      null,
      () => "t1",
    );
    const updated = buildRecord({ port: 2222 }, existing, () => "unused");
    expect(updated.id).toBe("t1");
    expect(updated.port).toBe(2222);
    expect(updated.hostname).toBe("box");
    expect(updated.key_id).toBe("k1");
  });
});

describe("store-backed target registry", () => {
  it("listTargets always includes local first, even with no remotes", () => {
    expect(listTargets()).toEqual([LOCAL_TARGET]);
  });

  it("listTargets sorts remotes by label after local", () => {
    putTarget(
      buildRecord(
        { hostname: "b", username: "u", key_id: "k1", label: "Bravo" },
        null,
        () => "t1",
      ),
    );
    putTarget(
      buildRecord(
        { hostname: "a", username: "u", key_id: "k2", label: "Alpha" },
        null,
        () => "t2",
      ),
    );
    const targets = listTargets();
    expect(targets.map((t) => t.id)).toEqual(["local", "t2", "t1"]);
  });

  it("getTarget resolves 'local' without touching the store", () => {
    expect(getTarget("local")).toEqual(LOCAL_TARGET);
    expect(getTarget("nope")).toBeNull();
  });

  it("resolveTarget matches by id, label, or hostname (case-insensitive)", () => {
    putTarget(
      buildRecord(
        {
          hostname: "Box.Example.com",
          username: "u",
          key_id: "k1",
          label: "MyBox",
        },
        null,
        () => "t1",
      ),
    );
    expect(resolveTarget("local").id).toBe("local");
    expect(resolveTarget("t1").id).toBe("t1");
    expect(resolveTarget("mybox").id).toBe("t1");
    expect(resolveTarget("box.example.com").id).toBe("t1");
    expect(() => resolveTarget("missing")).toThrow(/unknown target/);
    expect(() => resolveTarget("")).toThrow(/required/);
  });
});

describe("toConn", () => {
  it("builds an SshConn using Auth::KeyRef (key_id), never raw key material", () => {
    const rec = buildRecord(
      { hostname: "h", username: "u", key_id: "vault-key-1", port: 2222 },
      null,
      () => "t1",
    );
    const conn = toConn(rec);
    expect(conn).toEqual({
      host: "h",
      port: 2222,
      username: "u",
      auth: { key_id: "vault-key-1" },
    });
  });

  it("refuses to build a connection for the local target", () => {
    expect(() => toConn(LOCAL_TARGET)).toThrow(/not a remote/);
  });
});
