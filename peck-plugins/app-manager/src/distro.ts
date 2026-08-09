// Pure distro-detection logic: parse /etc/os-release and map it to one of
// the four package managers this plugin knows how to drive. No host calls —
// the caller is responsible for fetching the file content (locally or over
// SSH) via exec.ts and handing the raw text in here.

export type PackageManager = "apt" | "dnf" | "pacman" | "zypper";

export const OS_RELEASE_PROBE = "cat /etc/os-release 2>/dev/null";

export interface OsRelease {
  id: string;
  idLike: string[];
}

/** Parse the KEY=VALUE (with optional quoting) shape of /etc/os-release. */
export function parseOsRelease(text: string): OsRelease {
  const fields: Record<string, string> = {};
  for (const line of text.split("\n")) {
    const m = /^([A-Za-z_][A-Za-z0-9_]*)=(.*)$/.exec(line.trim());
    if (!m) continue;
    let value = m[2].trim();
    if (
      (value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"))
    ) {
      value = value.slice(1, -1);
    }
    fields[m[1]] = value;
  }
  return {
    id: (fields.ID || "").toLowerCase(),
    idLike: (fields.ID_LIKE || "").toLowerCase().split(/\s+/).filter(Boolean),
  };
}

const FAMILY_TO_PM: Record<string, PackageManager> = {
  debian: "apt",
  ubuntu: "apt",
  fedora: "dnf",
  rhel: "dnf",
  centos: "dnf",
  rocky: "dnf",
  almalinux: "dnf",
  arch: "pacman",
  manjaro: "pacman",
  suse: "zypper",
  opensuse: "zypper",
  "opensuse-leap": "zypper",
  "opensuse-tumbleweed": "zypper",
  sles: "zypper",
};

/**
 * Map an /etc/os-release `ID`/`ID_LIKE` pair to a package manager. Returns
 * `null` for anything unrecognised — callers must refuse cleanly, never
 * guess a command.
 */
export function detectPackageManager(
  release: OsRelease,
): PackageManager | null {
  const candidates = [release.id, ...release.idLike];
  for (const candidate of candidates) {
    const pm = FAMILY_TO_PM[candidate];
    if (pm) return pm;
  }
  return null;
}
