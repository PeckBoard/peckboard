import { describe, expect, it } from "vitest";
import {
  REQUEST_TOKEN,
  injectRequest,
  parseInstallRequest,
  requestLiteral,
  sanitizeFrom,
} from "../src/deeplink";

describe("parseInstallRequest", () => {
  it("reads the app list, source label, and target", () => {
    const req = parseInstallRequest(
      "install=python3,pip,graphifyy&from=graphify&target=local",
    );
    expect(req).toEqual({
      apps: ["python3", "pip", "graphifyy"],
      from: "graphify",
      target: "local",
    });
  });

  it("is null without an install param", () => {
    expect(parseInstallRequest("")).toBeNull();
    expect(parseInstallRequest("from=graphify")).toBeNull();
  });

  it("drops ids that are not catalog slugs, and dedupes", () => {
    const req = parseInstallRequest(
      "install=pip,PIP,pip,../etc/passwd,%3Cscript%3E,node",
    );
    // PIP lowercases to a duplicate of pip; the path and the tag are dropped.
    expect(req?.apps).toEqual(["pip", "node"]);
  });

  it("is null when every id was junk", () => {
    expect(parseInstallRequest("install=,,%20,../x")).toBeNull();
  });

  it("caps the list so a link cannot queue up an unbounded bar", () => {
    const many = Array.from({ length: 30 }, (_v, i) => `app${i}`).join(",");
    expect(parseInstallRequest(`install=${many}`)?.apps).toHaveLength(12);
  });

  it("ignores a target id that is not a slug", () => {
    expect(parseInstallRequest("install=pip&target=../other")?.target).toBe("");
  });
});

describe("sanitizeFrom", () => {
  it("keeps plain words and drops markup", () => {
    expect(sanitizeFrom("<b>graphify</b>")).toBe("bgraphifyb");
    expect(sanitizeFrom("Graphify 0.4.1")).toBe("Graphify 0.4.1");
    expect(sanitizeFrom(undefined)).toBe("");
  });

  it("truncates a novel-length label", () => {
    expect(sanitizeFrom("x".repeat(200))).toHaveLength(40);
  });
});

describe("requestLiteral", () => {
  it("serializes null for a normal page load", () => {
    expect(requestLiteral(null)).toBe("null");
  });

  it("escapes characters that could close the script element", () => {
    // `from` is already sanitized in practice; prove the encoder holds anyway.
    const lit = requestLiteral({
      apps: ["pip"],
      from: "</script><img>&",
      target: "",
    });
    expect(lit).not.toContain("<");
    expect(lit).not.toContain(">");
    expect(lit).not.toContain("&");
    expect(
      JSON.parse(
        lit
          .replace(/\\u003c/g, "<")
          .replace(/\\u003e/g, ">")
          .replace(/\\u0026/g, "&"),
      ),
    ).toEqual({ apps: ["pip"], from: "</script><img>&", target: "" });
  });
});

describe("injectRequest", () => {
  it("replaces the token with the literal", () => {
    const page = `<script>var REQ = ${REQUEST_TOKEN};</script>`;
    expect(
      injectRequest(page, { apps: ["pip"], from: "graphify", target: "" }),
    ).toBe(
      '<script>var REQ = {"apps":["pip"],"from":"graphify","target":""};</script>',
    );
  });

  it("leaves a normal load with a null literal", () => {
    expect(injectRequest(`var REQ = ${REQUEST_TOKEN};`, null)).toBe(
      "var REQ = null;",
    );
  });
});
