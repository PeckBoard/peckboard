import { describe, expect, it } from "vitest";
import {
  APPS,
  findApp,
  installRecipeFor,
  removeRecipeFor,
} from "../src/catalog";

const PMS = ["apt", "dnf", "pacman", "zypper"] as const;
const VENDOR_ONLY = ["claude", "cursor-agent", "ollama"];
const PM_APPS = ["git", "node", "docker", "ripgrep"];

describe("catalog", () => {
  it("includes at least the required apps", () => {
    const ids = APPS.map((a) => a.id);
    for (const id of [...PM_APPS, ...VENDOR_ONLY]) {
      expect(ids).toContain(id);
    }
  });

  it("findApp is case-sensitive and returns undefined for unknown ids", () => {
    expect(findApp("git")?.id).toBe("git");
    expect(findApp("GIT")).toBeUndefined();
    expect(findApp("nonexistent")).toBeUndefined();
  });

  it("every distro-packaged app has an install and remove recipe for all four package managers", () => {
    for (const id of PM_APPS) {
      const app = findApp(id)!;
      for (const pm of PMS) {
        expect(app.install[pm], `${id} install.${pm}`).toBeTruthy();
        expect(app.install[pm]).toContain("sudo -A");
        expect(app.remove[pm], `${id} remove.${pm}`).toBeTruthy();
        expect(app.remove[pm]).toContain("sudo -A");
      }
    }
  });

  it("vendor-only apps have a vendor install+remove script and no package-manager recipes", () => {
    for (const id of VENDOR_ONLY) {
      const app = findApp(id)!;
      expect(app.install.vendor, `${id} install.vendor`).toBeTruthy();
      expect(app.remove.vendor, `${id} remove.vendor`).toBeTruthy();
      for (const pm of PMS) {
        expect(
          app.install[pm],
          `${id} install.${pm} should be absent`,
        ).toBeUndefined();
      }
    }
  });

  it("every catalog app has a detect and version probe", () => {
    for (const app of APPS) {
      expect(app.detect.length).toBeGreaterThan(0);
      expect(app.version.length).toBeGreaterThan(0);
    }
  });

  it("installRecipeFor prefers the package-manager recipe over vendor", () => {
    const git = findApp("git")!;
    expect(installRecipeFor(git, "apt")).toBe(git.install.apt);
  });

  it("installRecipeFor falls back to vendor when there's no recipe for the detected pm", () => {
    const ollama = findApp("ollama")!;
    expect(installRecipeFor(ollama, "apt")).toBe(ollama.install.vendor);
    expect(installRecipeFor(ollama, null)).toBe(ollama.install.vendor);
  });

  it("installRecipeFor/removeRecipeFor return null when neither a pm recipe nor vendor exists", () => {
    const fake = {
      id: "x",
      name: "x",
      description: "",
      detect: "",
      version: "",
      install: {},
      remove: {},
    };
    expect(installRecipeFor(fake, "apt")).toBeNull();
    expect(removeRecipeFor(fake, null)).toBeNull();
  });
});
