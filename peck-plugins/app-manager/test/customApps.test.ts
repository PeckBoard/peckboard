// Manually added apps: validation of what a person types, the projection into
// the CatalogApp shape everything else consumes, and the store round trip.
//
// The load-bearing checks here are the ones that keep a typed name from
// becoming a shell surprise: the probe is built from a validated token and
// shell-quoted, and a command is only ever the verbatim string the person
// entered — never assembled from the name, binary or homepage.

import { beforeEach, describe, expect, it } from "vitest";
import { installHost } from "./hostShim";
import {
  acceptSuggestion,
  allApps,
  applyResearchDetails,
  buildCustomApp,
  customAppView,
  describeCustomApp,
  discardSuggestion,
  findAnyApp,
  forgetCustomApp,
  getCustomApp,
  listCustomApps,
  missingDetails,
  putCustomApp,
  slugify,
  toCatalogApp,
} from "../src/customApps";

describe("validation", () => {
  beforeEach(() => installHost({}));

  it("slugs the name into an id and defaults the probe binary to it", () => {
    const rec = buildCustomApp({ name: "Zellij" });
    expect(rec.id).toBe("zellij");
    expect(rec.binary).toBe("zellij");
    expect(rec.name).toBe("Zellij");
    expect(rec.install_command).toBeUndefined();
    expect(rec.remove_command).toBeUndefined();
  });

  it("slugifies punctuation and spacing", () => {
    expect(slugify("GitHub CLI (gh)")).toBe("github-cli-gh");
    expect(slugify("  uv  ")).toBe("uv");
    expect(slugify("!!!")).toBe("");
  });

  it("refuses a name that yields no usable id", () => {
    expect(() => buildCustomApp({ name: "!!!" })).toThrow(/usable id/);
  });

  it("refuses a name that collides with a catalog app", () => {
    expect(() => buildCustomApp({ name: "git" })).toThrow(/already exists/);
  });

  it("refuses a name that collides with an existing manual app", () => {
    putCustomApp(buildCustomApp({ name: "Zellij" }));
    expect(() => buildCustomApp({ name: "zellij" })).toThrow(/already exists/);
  });

  it("refuses a binary that isn't a bare command name", () => {
    expect(() =>
      buildCustomApp({ name: "Zellij", binary: "zellij; rm -rf /" }),
    ).toThrow(/valid command name/);
    expect(() => buildCustomApp({ name: "Zellij", binary: "$(id)" })).toThrow(
      /valid command name/,
    );
  });

  it("refuses a homepage that isn't https", () => {
    expect(() =>
      buildCustomApp({ name: "Zellij", homepage: "http://zellij.dev" }),
    ).toThrow(/https/);
    expect(() =>
      buildCustomApp({ name: "Zellij", homepage: "javascript:alert(1)" }),
    ).toThrow(/https/);
    expect(
      buildCustomApp({ name: "Zellij", homepage: "https://zellij.dev" })
        .homepage,
    ).toBe("https://zellij.dev");
  });

  it("keeps a command verbatim but refuses a multi-line one", () => {
    const rec = buildCustomApp({
      name: "Zellij",
      install_command: "sudo -A apt-get install -y zellij",
    });
    expect(rec.install_command).toBe("sudo -A apt-get install -y zellij");
    expect(() =>
      buildCustomApp({ name: "Other", install_command: "a\nb" }),
    ).toThrow(/single line/);
  });

  it("keeps the id and unspecified fields on an edit", () => {
    const first = buildCustomApp({
      name: "Zellij",
      binary: "zellij",
      notes: "terminal multiplexer",
      install_command: "cargo install zellij",
    });
    const edited = buildCustomApp(
      { remove_command: "rm -f ~/bin/zellij" },
      first,
    );
    expect(edited.id).toBe("zellij");
    expect(edited.name).toBe("Zellij");
    expect(edited.notes).toBe("terminal multiplexer");
    expect(edited.install_command).toBe("cargo install zellij");
    expect(edited.remove_command).toBe("rm -f ~/bin/zellij");
    expect(edited.created_at).toBe(first.created_at);
  });
});

describe("projection into the catalog shape", () => {
  beforeEach(() => installHost({}));

  it("builds shell-quoted probes and no recipe when no command was given", () => {
    const app = toCatalogApp(buildCustomApp({ name: "Zellij" }));
    expect(app.custom).toBe(true);
    expect(app.detect).toBe("command -v 'zellij'");
    expect(app.version).toContain("'zellij' --version");
    expect(app.install).toEqual({});
    expect(app.remove).toEqual({});
  });

  it("carries the person's commands through as the vendor recipes", () => {
    const app = toCatalogApp(
      buildCustomApp({
        name: "Zellij",
        install_command: "sudo -A apt-get install -y zellij",
        remove_command: "sudo -A apt-get remove -y zellij",
      }),
    );
    expect(app.install.vendor).toBe("sudo -A apt-get install -y zellij");
    expect(app.remove.vendor).toBe("sudo -A apt-get remove -y zellij");
  });

  it("describes an app with no notes honestly, and names the site when given", () => {
    const bare = describeCustomApp(buildCustomApp({ name: "Zellij" }));
    expect(bare).toContain("Manually added");
    expect(bare).toContain("official source");
    const withSite = describeCustomApp(
      buildCustomApp({
        name: "Zellij",
        notes: "terminal multiplexer",
        homepage: "https://zellij.dev",
      }),
    );
    expect(withSite).toBe(
      "terminal multiplexer Official site: https://zellij.dev",
    );
  });
});

describe("the store and the combined app set", () => {
  beforeEach(() => installHost({}));

  it("round-trips a record and appends it after the catalog", () => {
    putCustomApp(buildCustomApp({ name: "Zellij" }));
    expect(listCustomApps().map((r) => r.id)).toEqual(["zellij"]);
    expect(getCustomApp("zellij")?.name).toBe("Zellij");

    const all = allApps();
    expect(all[0].id).toBe("git");
    expect(all[all.length - 1].id).toBe("zellij");
    expect(findAnyApp("zellij")?.custom).toBe(true);
    expect(findAnyApp("git")?.custom).toBeUndefined();
    expect(findAnyApp("nope")).toBeUndefined();
  });

  it("forgets a record without touching anything else", () => {
    putCustomApp(buildCustomApp({ name: "Zellij" }));
    expect(forgetCustomApp("zellij")).toBe(true);
    expect(forgetCustomApp("zellij")).toBe(false);
    expect(listCustomApps()).toEqual([]);
    expect(allApps().some((a) => a.custom)).toBe(false);
  });

  it("drops a stored app whose id a later catalog release has taken", () => {
    // Written straight to the store: buildCustomApp would refuse the id today,
    // but a catalog entry added in a later release can still collide with one
    // saved before it existed.
    putCustomApp({ id: "git", name: "My Git", binary: "git", notes: "" });
    const ids = allApps().map((a) => a.id);
    expect(ids.filter((id) => id === "git")).toHaveLength(1);
    expect(findAnyApp("git")?.custom).toBeUndefined();
  });
});

// The rules that keep an AI session from writing over someone's own entry,
// or from arming shell this plugin would later run verbatim on a target.
describe("filling the blanks in from a session's findings", () => {
  beforeEach(() => installHost({}));

  it("fills blank notes, website and derived binary", () => {
    const rec = buildCustomApp({ name: "Zellij" });
    const out = applyResearchDetails(rec, {
      notes: "terminal multiplexer",
      homepage: "https://zellij.dev",
      binary: "zellij-bin",
    });
    expect(out.rec.notes).toBe("terminal multiplexer");
    expect(out.rec.homepage).toBe("https://zellij.dev");
    expect(out.rec.binary).toBe("zellij-bin");
    // No longer a guess off the id, so it is not offered for filling again.
    expect(out.rec.binary_derived).toBeUndefined();
    expect(out.applied).toEqual([
      "notes",
      "official website",
      "detect command",
    ]);
    expect(out.rec.filled_fields).toEqual(out.applied);
    expect(out.skipped).toEqual([]);
  });

  it("never overwrites what the person typed, and says which values it dropped", () => {
    const rec = buildCustomApp({
      name: "Zellij",
      binary: "zellij",
      notes: "mine",
      homepage: "https://example.com",
      install_command: "cargo install zellij",
    });
    const out = applyResearchDetails(rec, {
      notes: "theirs",
      homepage: "https://zellij.dev",
      binary: "something-else",
      install_command: "sudo -A apt-get install -y zellij",
    });
    expect(out.rec.notes).toBe("mine");
    expect(out.rec.homepage).toBe("https://example.com");
    expect(out.rec.binary).toBe("zellij");
    expect(out.rec.install_command).toBe("cargo install zellij");
    expect(out.rec.suggested_install_command).toBeUndefined();
    expect(out.applied).toEqual([]);
    expect(out.skipped).toEqual([
      "notes",
      "official website",
      "detect command",
      "install command",
    ]);
  });

  it("parks a proposed command as a suggestion, never as a runnable recipe", () => {
    const rec = buildCustomApp({ name: "Zellij" });
    const out = applyResearchDetails(rec, {
      install_command: "sudo -A apt-get install -y zellij",
      remove_command: "sudo -A apt-get remove -y zellij",
    });
    expect(out.rec.install_command).toBeUndefined();
    expect(out.rec.remove_command).toBeUndefined();
    expect(out.rec.suggested_install_command).toBe(
      "sudo -A apt-get install -y zellij",
    );
    expect(out.suggested).toEqual(["install command", "remove command"]);
    // The projection the jobs run from must not see a pending suggestion.
    const app = toCatalogApp(out.rec);
    expect(app.install).toEqual({});
    expect(app.remove).toEqual({});
  });

  it("applies the same validation to a session's values as to a person's", () => {
    const rec = buildCustomApp({ name: "Zellij" });
    expect(() =>
      applyResearchDetails(rec, { binary: "zellij; rm -rf /" }),
    ).toThrow(/valid command name/);
    expect(() =>
      applyResearchDetails(rec, { homepage: "http://zellij.dev" }),
    ).toThrow(/https/);
    expect(() =>
      applyResearchDetails(rec, { install_command: "a\nb" }),
    ).toThrow(/single line/);
  });

  it("makes a suggestion runnable only on accept, and drops it on discard", () => {
    const rec = applyResearchDetails(buildCustomApp({ name: "Zellij" }), {
      install_command: "cargo install zellij",
      remove_command: "rm -f ~/.cargo/bin/zellij",
    }).rec;

    const accepted = acceptSuggestion(rec, "install");
    expect(accepted.install_command).toBe("cargo install zellij");
    expect(accepted.suggested_install_command).toBeUndefined();
    expect(toCatalogApp(accepted).install.vendor).toBe("cargo install zellij");

    const discarded = discardSuggestion(accepted, "remove");
    expect(discarded.remove_command).toBeUndefined();
    expect(discarded.suggested_remove_command).toBeUndefined();
    expect(() => discardSuggestion(discarded, "remove")).toThrow(
      /no suggested remove command/,
    );
    expect(() => acceptSuggestion(discarded, "remove")).toThrow(
      /no suggested remove command/,
    );
  });

  it("keeps an unanswered suggestion across an edit that didn't answer it", () => {
    const rec = applyResearchDetails(buildCustomApp({ name: "Zellij" }), {
      install_command: "cargo install zellij",
    }).rec;
    const edited = buildCustomApp({ notes: "terminal multiplexer" }, rec);
    expect(edited.suggested_install_command).toBe("cargo install zellij");
    // ...but typing the command yourself answers it.
    const typed = buildCustomApp({ install_command: "my own" }, rec);
    expect(typed.install_command).toBe("my own");
    expect(typed.suggested_install_command).toBeUndefined();
  });

  it("reports what is still blank, counting a pending suggestion as answered", () => {
    const bare = buildCustomApp({ name: "Zellij" });
    expect(missingDetails(bare)).toEqual([
      "notes",
      "official website",
      "detect command",
      "install command",
      "remove command",
    ]);
    const filled = applyResearchDetails(bare, {
      notes: "terminal multiplexer",
      homepage: "https://zellij.dev",
      binary: "zellij",
      install_command: "cargo install zellij",
    }).rec;
    expect(missingDetails(filled)).toEqual(["remove command"]);

    const view = customAppView(filled);
    expect(view.missing).toEqual(["remove command"]);
    expect(view.suggestions).toEqual([
      {
        field: "install",
        label: "install command",
        command: "cargo install zellij",
      },
    ]);
  });

  it("treats a binary typed by hand as the person's, even from an older record", () => {
    // Saved before `binary_derived` existed: no flag, so it counts as typed.
    const legacy = {
      id: "zellij",
      name: "Zellij",
      binary: "zellij",
      notes: "",
    };
    const out = applyResearchDetails(legacy, { binary: "other" });
    expect(out.rec.binary).toBe("zellij");
    expect(out.skipped).toEqual(["detect command"]);
  });
});
